# Plano de Implementação: aihub (Multi-Harness Terminal Supervisor & Router)

> Localização: este repositório (`github.com/MathBorgess/ai-hub`), com as crates na raiz. O rascunho original apontava para `~/.gemini/antigravity/scratch/aihub`.

O aihub é uma camada de terminal interativa e desacoplada, escrita em Rust, que orquestra vários coding agent harnesses (Antigravity CLI `agy`, Claude Code `claude`, OpenAI Codex `codex` e Cursor Agent `cursor-agent`) numa única interface contínua.

Ele junta três coisas: a memória persistente e padronizada do `ai-memory` (AkitaOnRails), a leitura de quotas por janela e por lane da skill `handoff`, e o isolamento em Git Worktrees dessa mesma skill. Tudo roda numa arquitetura cliente-servidor (`aihubd` + TUI `aihub`).

## 1. Visão geral da arquitetura

São dois processos, ligados por Unix Domain Socket (`~/.local/share/aihub/aihub.sock`):

```
┌───────────────────────────────────────────────────────────────┐
│                          aihub (TUI)                          │
│  [Statusline htop: Quotas/Lanes] [Modo: Assistido/Autônomo]   │
│  [PTY Interativo: Streaming ANSI] [Palette Ctrl+P: /switch]   │
└──────────────────────────────▲────────────────────────────────┘
                               │ IPC (Unix Domain Socket)
┌──────────────────────────────▼────────────────────────────────┐
│                       aihubd (Daemon)                         │
│  ├─ aihub-probe: Probing nativo (Keychain, SQLite, RPC ports) │
│  ├─ aihub-router: Classificador Híbrido (Regras + LLM leve)   │
│  ├─ aihub-pty: Hospedeiro de PTYs virtuais (portable-pty)     │
│  ├─ aihub-git: Worktree Shadow Manager (.git/worktrees/)      │
│  └─ aihub-memory: Bridge com ai-memory (MCP / Handoff Brief)  │
└───────────────────────────────────────────────────────────────┘
          │                   │                 │
    [Claude Code]      [Antigravity]      [Cursor / Codex]
```

O que essa topologia garante:

1. **A sessão sobrevive ao terminal.** Fechar o terminal não interrompe o agente, que segue rodando no daemon. `aihub attach` reconecta na hora.
2. **A TUI abre com as quotas já prontas.** O daemon atualiza as quotas em segundo plano, então não há espera de probe na abertura.
3. **O código dos agentes fica isolado.** Todo trabalho roda em Git Worktrees isoladas em `${TMPDIR}/aihub/worktrees/<session-id>`, e o merge é assistido quando a tarefa termina.

## 2. Estrutura do workspace Cargo

```
aihub/
├── Cargo.toml                  # Workspace root
├── aihub-core/                 # Tipos de domínio, eventos de IPC, serialização
├── aihub-probe/                # Motor nativo de quotas, janelas (5h, 7d) e lanes
├── aihub-router/               # Classificador de tarefas (tiers), roteador e policies
├── aihub-pty/                  # Gerenciador de PTY virtual e forwarder de ANSI
├── aihub-git/                  # Gerenciamento de Git Worktrees e diffs
├── aihub-memory/               # Integração com ai-memory e gerador de Handoff Brief
├── aihubd/                     # Binário do Daemon (Tokio event loop, socket server)
└── aihub/                      # Binário da TUI (Ratatui, Crossterm, terminal client)
```

## 3. Componentes

### 3.1. `aihub-core`

Os dados canônicos do sistema:

- `HarnessId`: `ClaudeCode`, `Antigravity`, `Codex`, `CursorAgent`.
- `TaskTier`: `Mechanical` (implementação, lint, testes), `Design` (arquitetura, decisões de alto nível), `Review` (auditoria, diffs, somente leitura).
- `TaskSize`: `S` (1–2 arquivos), `M` (um módulo), `L` (um subsistema).
- `QuotaSnapshot`: as janelas ativas (`five_hour`, `seven_day`), o percentual gasto, o tempo até o reset (`resets_in_s`) e o status (`Ok`, `Low`, `Empty`, `Unknown`).
- `Lane`: no Cursor, `cursor-models` e `other-models`; no Antigravity, `gemini` e `third-party`.
- Mensagens de IPC: `ClientMessage` e `DaemonMessage`, que carregam o stream de bytes do PTY e os comandos de controle.

### 3.2. `aihub-probe` (reescrita nativa, 100% Rust)

Porta toda a lógica de probe do `handoff.mjs`, eliminando a dependência do runtime Node.js:

- **macOS Keychain:** a crate `security-framework` lê a credencial `Claude Code-credentials` (serviço do Keychain) e a sessão `gemini` / `antigravity`.
- **SQLite do Cursor IDE:** consulta assíncrona ao `state.vscdb` com `rusqlite`, para extrair o JWT `cursorAuth/accessToken`. Com ele, dispara um request HTTP em Connect-Protocol para `api2.cursor.sh/.../GetCurrentPeriodUsage`.
- **Portas RPC do Antigravity:** varre os processos locais `agy`, `antigravity` e `language_server` para descobrir as portas RPC efêmeras, via chamadas de sistema ou `lsof`. Depois dispara JSON-RPC local para `RetrieveUserQuotaSummary`.
- **Auth do Codex:** lê o token em `~/.codex/auth.json` e consulta o endpoint de uso.
- **Fallback local (transcripts):** um parser rápido de JSONL sobre os diretórios de log locais, que compara a janela móvel de 5 horas com a semana mais pesada.

### 3.3. `aihub-router` (roteamento híbrido)

- **Heurísticas locais rápidas (<2 ms):** classificação estática por regex e por verbo de ação:
  - `Mechanical`: "refactor", "rename", "fix typo", "add unit test", "lint", "format".
  - `Review`: "check diff", "audit", "security review", "explain", "find bug".
  - `Design`: "architect", "design", "plan", "RFC" e especificações ambíguas.
- **Fallback por LLM rápido:** se o prompt for longo ou ambíguo, uma chamada assíncrona curta (Gemini Flash ou Claude Haiku) classifica o tier e sugere o harness.
- **Resolução de slots e lanes:**
  - Cada tier vai para a sua lane ideal: `Mechanical` para a lane `own` (Cursor Models ou Antigravity Gemini), `Design` para a lane `frontier` (Claude Code ou Antigravity Third-Party).
  - Antes de escolher, o router consulta o `aihub-probe`. Slots e lanes com quota esgotada ficam de fora, e os que precisam esperar o reset da janela recebem penalidade (`holds X min`).

### 3.4. `aihub-git` (Worktree Shadow Manager)

- Cria branches efêmeras `session/<run-id>` a partir da branch atual do usuário.
- Provisiona uma Git Worktree dedicada em `${TMPDIR}/aihub/worktrees/<session-id>`.
- Na troca de harness (por exemplo, de Claude Code para Antigravity), o novo harness abre na worktree que já existe, então os arquivos alterados continuam onde estavam.
- Ao fim da sessão, mostra um diff colorido e oferece três saídas:
  - `Fast-Forward / Squash Merge` na branch original.
  - `Keep Branch`, para revisão manual depois.
  - `Discard`: descarte limpo, sem afetar o repositório.

### 3.5. `aihub-memory` (ponte com `ai-memory` + protocolo de handoff)

Em segundo plano, se conecta ao daemon `ai-memory` (lê o SQLite `memory.sqlite` e a wiki Git). Na troca de harness:

1. Extrai o último turno e as decisões do harness que está saindo.
2. Gera o Handoff Brief compacto (`NN.prompt.md` e `NN.md`), com objetivos e restrições bem definidos.
3. Injeta o brief na inicialização do novo harness (`--prompt` ou arquivo de instrução de inicialização).
4. Registra o handoff no `ai-memory`, para auditoria e persistência de longo prazo.

### 3.6. `aihubd` (daemon)

- Servidor Tokio escutando no Unix Domain Socket `~/.local/share/aihub/aihub.sock`.
- Gerencia o ciclo de vida das instâncias de `portable-pty`.
- Mantém um loop de telemetria: atualiza as quotas a cada 2 minutos, ou sob demanda.
- Aceita vários clientes e reconecta sem que o usuário perceba.

### 3.7. `aihub` (cliente TUI)

Interface de terminal em Ratatui e Crossterm:

- **Header / statusline:**
  - Barras coloridas de quota (Claude, Antigravity Gemini/3p, Cursor).
  - O modo ativo: `[ASSISTIDO]` ou `[AUTÔNOMO]`.
  - O harness atual e a branch da worktree.
- **Corpo central:** um emulador de terminal virtual que renderiza, em tempo real, o stream do PTY do agente ativo.
- **Footer / command palette (`Ctrl+P` ou `:`):** atalhos de controle rápido.
  - `Enter`: aceita a recomendação de troca no modo assistido.
  - `Tab`: alterna entre os harnesses disponíveis.
  - `Ctrl+M`: alterna entre os modos Assistido e Autônomo.
  - `/switch <harness>`: força a troca para outro harness, com handoff automático.
  - `/merge`: conclui a tarefa e inicia a fusão da worktree.
  - `/quota`: mostra a tabela analítica completa de janelas e lanes.

## 4. Plano de verificação

### Testes automatizados

- **`aihub-probe`, testes unitários:** mocks do Keychain e das respostas dos endpoints validam o cálculo das janelas (5h, 7d) e a escolha de lane.
- **`aihub-router`, testes de roteamento:** uma bateria de prompts confirma que comandos mecânicos recebem o tier `Mechanical` e que arquitetura recebe `Design`.
- **`aihub-git`, testes de isolamento:** criar, escrever e remover Git Worktrees temporárias sem corromper o repositório principal.

### Verificação manual ponta a ponta

1. **Daemon:** rodar `aihubd` em background e conferir que o socket Unix foi criado.
2. **TUI e quotas:** rodar `aihub` num repositório git e conferir, na statusline, as quotas reais lidas do macOS Keychain e do Antigravity.
3. **Execução interativa:** iniciar uma tarefa simples no Claude Code ou no Antigravity pela TUI e conferir que comandos e cores passam normalmente pelo PTY.
4. **Detach/attach:** fechar a janela do terminal no meio da execução, reabrir com `aihub attach` e confirmar que a sessão não foi interrompida.
5. **Troca e handoff:** disparar `/switch agy` (ou aceitar o banner do modo assistido) e conferir que o Antigravity abre com o Handoff Brief e o resumo do que o Claude Code fez.
6. **Finalização e merge:** disparar `/merge` e conferir o diff da worktree e o squash merge na branch principal.
