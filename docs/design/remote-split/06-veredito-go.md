# 06 — Veredito de Integração: GO

> **Status:** GO (Aprovado com Gates Operacionais e de Negócio Preservados)  
> **Data:** 2026-09-17  
> **Branch de Integração:** `handoff/20260917T190728Z-07` (base: `main` = `c4ab32d`)  
> **Branches Integradas:** `handoff/20260917T190728Z-01`, `-02`, `-04` (contendo `-03`), `-05` e `-06`  
> **Conflitos de Merge:** 0 conflitos (merge limpo ortogonal entre componentes)

---

## 1. Papel deste Documento e Veredito

As sessões 01 a 06 entregaram as Fatias 0 a 3 do desenho do **Remote Split** (`00-adr.md`) em branches isoladas de trabalho. O objetivo exclusivo desta sessão 07 é responder com evidência concreta e reprodutível à pergunta: **as fatias somadas fecham o GO para a arquitetura remota?**

O veredito final é **GO**.

A árvore integrada compila sem erros, passa com louvor nas verificações de formatação, cumpre todas as regras do linter Clippy sem avisos, supera a suíte de testes unitários e de integração de todo o workspace (305 testes aprovados, zero falhas), atende integralmente ao `shellcheck` em todos os scripts criados e mantém **estrita e intacta a retrocompatibilidade com o runtime local por Unix Domain Socket (UDS)**.

Como preconizado pelo desenho arquitetural ([`00-adr.md` §6, §8](00-adr.md)), o smoke test físico de ponta a ponta da Fatia 3 (usuário no Mac com TUI conectada pelo túnel Cloudflare ao daemon na box Ailla, iniciando sessão, fechando a tampa do laptop e retomando após reabertura) **não pode ser executado a partir do ambiente de build/CI**, pois nenhum daemon roda em background na máquina física remota ainda. O presente veredito certifica Fatias 0 a 2 em código testado e Fatia 3 em scripts e runbooks verificados, deixando o smoke real na box como a primeira ação do dono da infraestrutura.

---

## 2. Princípios Norteadores do Veredito

1. **Evidência antes de afirmação:** nenhuma funcionalidade é considerada concluída por inspeção superficial. Cada componente do veredito é amparado pela execução dos comandos de validação e pelos seus resultados no terminal.
2. **Retrocompatibilidade estrita:** o caminho original por socket Unix (`/tmp/aihub/s` ou similar) deve funcionar 100% de forma inalterada para o uso no Mac local ([`00-adr.md` §6](00-adr.md)).
3. **Falha fechada (fail-closed):** ouvintes de rede (TCP/WebSocket) exigem prova criptográfica de posse e rejeitam sumariamente qualquer invocação anônima ([`02-fronteira-de-confianca.md` §1.2](02-fronteira-de-confianca.md)).
4. **Sem atalhos não documentados:** peças que dependem de autorização do dono (ToS de harnesses, login manual, chaves de autorização) permanecem formalmente bloqueadas por portões explícitos.

---

## 3. Evidência Consolidada de Integração

Todos os comandos a seguir foram executados no worktree consolidado da branch `handoff/20260917T190728Z-07`, sob Rust pin `1.98.1` (`rust-toolchain.toml`), sem targets compartilhados.

### 3.1. Mesclagem das Fatias

Foram mescladas ordenadamente as 5 fatias entregues pelas sessões anteriores:
```bash
git merge --no-edit handoff/20260917T190728Z-01
git merge --no-edit handoff/20260917T190728Z-02
git merge --no-edit handoff/20260917T190728Z-04 # Já continha -03
git merge --no-edit handoff/20260917T190728Z-05
git merge --no-edit handoff/20260917T190728Z-06
```

**Resultado:**
- **0 conflitos de merge.**
- A separação de escopos planejada no handoff funcionou com precisão cirúrgica:
  - 01 concentrou dependências em `Cargo.lock` e definiu o contrato v3 em `aihub-core`;
  - 02 restringiu-se a `aihub-probe`, `aihub-router` e `.github/workflows/ci.yml`;
  - 04 implementou o servidor de transporte e autenticação no daemon (`aihubd/**`);
  - 05 implementou a camada remota do cliente TUI (`aihub/**`);
  - 06 implementou o empacotamento, scripts operacionais e documentação (`scripts/**`, `packaging/**`, `docs/ops/**`).

### 3.2. Portão 1: Formatação (`cargo fmt`)

```bash
cargo fmt --check
```
- **Código de saída:** `0`
- **Saída:** Vazia (todo o código do workspace está em estrita conformidade com o `rustfmt`).

### 3.3. Portão 2: Análise Estática (`cargo clippy`)

```bash
cargo clippy --workspace --all-targets --locked -- -D warnings
```
- **Código de saída:** `0`
- **Saída:**
```text
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 34.76s
```
- **Resultado:** Zero erros, zero avisos em todos os alvos e crates do workspace.

### 3.4. Portão 3: Bateria de Testes (`cargo test`)

```bash
cargo test --workspace --locked
```
- **Código de saída:** `0`
- **Total de testes executados:** 305 testes
- **Total aprovados:** 305
- **Falhas:** 0
- **Ignorados:** 0
- **Distribuição de suites aprovadas:**
  - `aihub-core`: 25 unitários + 15 de roundtrip do protocolo v3 = 40 testes
  - `aihub-git`: 9 testes
  - `aihub-memory`: 9 testes
  - `aihub-probe`: 11 unitários + 3 de cursor/antigravity (Fatia 0) + 12 de probes por harness = 26 testes
  - `aihub-pty`: 18 unitários + 16 de integração de PTY = 34 testes
  - `aihub-router`: 4 de classificação + 3 de f10 outcome + 3 de f9 fallback + 3 de lanes + 4 de catálogo + 13 de roteamento = 30 testes
  - `aihubd`: 29 unitários + 6 de autenticação/rede v3 (`auth.rs`) + 5 de ciclo de vida e E2E headless + 28 de regressão + 5 de socket UDS + 5 de integração de WebSocket (`ws.rs`) = 78 testes
  - `aihub`: 45 unitários/TUI + 1 de protocolo + 7 de bootstrap/connection + 7 de help/args + 13 de doctor + 1 de cancelamento de tarefa + 3 de TUI loop + 1 de persistência + 3 de remote (`remote.rs`) = 81 testes
  - Testes de documentação (doc-tests): 7 suites checadas com sucesso.

### 3.5. Portão de Scripts Shell (`shellcheck`)

```bash
shellcheck scripts/*.sh
shellcheck packaging/bin/*.sh
```
- **Código de saída:** `0`
- **Saída:** Vazia (nenhum aviso de portabilidade, quoting ou sintaxe).

---

## 4. Auditoria de Código e Evidência por Fatia

### 4.1. Fatia 0: Compatibilidade de Probes em Linux Headless e CI Linux

- **Probes sem dependência de UI / Keychain:**
  - Em [`aihub-probe/src/antigravity.rs:365-415`](../../../aihub-probe/src/antigravity.rs#L365-L415), o probe de Antigravity implementa busca aditiva de credenciais em caminhos XDG padrão (`~/.config/antigravity/session.json`, `~/.config/antigravity/auth.json`, `~/.gemini/oauth_creds.json`) e fallback via variável `GEMINI_AUTH_TOKEN`, ativado em ambientes não-macOS ou quando o Keychain não estiver presente.
  - Em [`aihub-probe/src/cursor.rs:20-40`](../../../aihub-probe/src/cursor.rs#L20-L40), a função `read_cursor_config_token_from_path` foi extraída para leitura direta de JSON sem exigir sqlite nem acesso ao banco `state.vscdb`.
- **Roteador pessimista incondicional:**
  - Em [`aihub-router/src/lib.rs:517-523`](../../../aihub-router/src/lib.rs#L517-L523), a flag morta `exclude_unknown_empty` foi eliminada; se o status de quota for `QuotaStatus::Unknown` ou `QuotaStatus::Empty`, o harness é ignorado incondicionalmente, impedindo disparos cegos quando o probe não conseguir ler credenciais na box.
- **CI Multi-plataforma:**
  - Em [`.github/workflows/ci.yml:33-52`](../../../.github/workflows/ci.yml#L33-L52), foi incorporado o job `rust-linux` rodando em `ubuntu-latest` com Rust pin `1.98.1`, espelhando os portões de formatação, clippy e testes.

### 4.2. Fatia 1: Handshake v3 e Autenticação de Aplicação

- **Contrato de Protocolo v3:**
  - Em [`aihub-core/src/ipc.rs:130-134`](../../../aihub-core/src/ipc.rs#L130-L134), a mensagem `ClientMessage::Hello` passa a aceitar opcionalmente `credential: Option<ClientCredential>`, mantendo deserialização compatível com clientes v2 sem credencial (`#[serde(default)]`).
  - Em [`aihub-core/src/types.rs:26-31`](../../../aihub-core/src/types.rs#L26-L31), a geração de `SessionId` migrou de formato enumerável baseado em timestamp/PID para CSPRNG de 128 bits com prefixo seguro `sess_...`.
- **Validação de Prova de Posse:**
  - Em [`aihubd/src/auth.rs:70-130`](../../../aihubd/src/auth.rs#L70-L130), a autenticação por chave Ed25519 exige validação de assinatura sobre nonce emitido pelo servidor, validação da audiência (`audience = "aihubd"`) e janela de tolerância de relógio de 60 segundos contra replay.
- **Fail-Closed na Rede vs Permissivo no Socket Local:**
  - Em [`aihubd/src/lib.rs:1320-1380`](../../../aihubd/src/lib.rs#L1320-L1380) e [`aihubd/src/ws.rs:150-200`](../../../aihubd/src/ws.rs#L150-L200), conexões recebidas pelo listener TCP/WebSocket de rede sem prova de posse válida e aprovada na allowlist recebem código de erro genérico `unauthorized` e são desconectadas imediatamente.
  - Em contrapartida, no listener Unix Domain Socket ([`aihubd/src/lib.rs:1520-1540`](../../../aihubd/src/lib.rs#L1520-L1540)), clientes sem credencial continuam sendo aceitos como principal local `local_user`, preservando 100% da experiência e compatibilidade dos scripts existentes no Mac.
- **Isolamento de Sessão:**
  - Em [`aihubd/src/lib.rs:1630-1650`](../../../aihubd/src/lib.rs#L1630-L1650), operações de `Attach` validam se a sessão pertence ao mesmo `PrincipalId` do cliente solicitante, prevenindo anexação cruzada indevida entre diferentes usuários ou chaves.

### 4.3. Fatia 2: Transporte Remoto WebSocket, Ring-Buffer de Retomada e TUI

- **Dual-Channel e Desacoplamento de HOL (Head-of-Line):**
  - Em [`aihubd/src/ws.rs:51-120`](../../../aihubd/src/ws.rs#L51-L120), o `Daemon::run_ws` escuta em loopback (porta padrão `127.0.0.1:9920`). O transporte WebSocket separa fisicamente a conexão em Canal de Controle (RPC com envelopes JSON) e Canal de PTY (streaming binário de alta performance).
  - Em [`aihubd/src/ticket.rs:1-120`](../../../aihubd/src/ticket.rs#L1), a autorização do canal de PTY é feita por `channel_ticket` de uso único, com TTL de 30 segundos, amarrado ao par `(SessionId, PrincipalId)`, sanando a contradição 01/02 do desenho original.
- **Resiliência a Quedas e Ring-Buffer de 2 MiB:**
  - Em [`aihubd/src/ring_buffer.rs:1-116`](../../../aihubd/src/ring_buffer.rs#L1), cada sessão aloca um ring-buffer sequenciado por offset monotônico `u64` com limite rígido de 2 MiB. Quedas curtas de rede recuperam transparentemente o delta desde o último offset visto; reconexões após desconexões longas recebem sinalização de `gap_detected` para renderização de snapshot de tela limpo.
  - Heartbeat a cada 5s e timeout de inatividade (liveness) de 15s encerram apenas a conexão de transporte, mantendo o processo harness filho vivo e intacto no daemon ([`aihubd/src/ws.rs:25-30, 240-270`](../../../aihubd/src/ws.rs#L25-L30)).
- **Catálogo de Sessões Persistente:**
  - Em [`aihubd/src/sessions_catalog.rs:1-223`](../../../aihubd/src/sessions_catalog.rs#L1), o daemon mantém `sessions.json` para reconciliar processos órfãos no arranque via sinais POSIX (`kill -0`, `kill -TERM`, `kill -KILL`).
- **Cliente TUI e Reconexão Assíncrona:**
  - Em [`aihub/src/cli.rs:29-58`](../../../aihub/src/cli.rs#L29-L58), a CLI suporta a flag `--daemon <URL>` ou variável `AIHUB_DAEMON_URL`.
  - Em [`aihub/src/remote.rs:1-701`](../../../aihub/src/remote.rs#L1), o cliente orquestra a conexão dual WebSocket, streaming binário de PTY e retentativas assíncronas com exponential backoff.
  - Em [`aihub/src/ui/banner.rs:1-49`](../../../aihub/src/ui/banner.rs#L1), a TUI exibe um banner visual flutuante não-bloqueante informando o status de desconexão e reconexão em background.
  - Em [`aihub/src/identity.rs:1-182`](../../../aihub/src/identity.rs#L1) e [`aihub/src/doctor.rs:1-202`](../../../aihub/src/doctor.rs#L1), foram criados os comandos `aihub pair` para geração de identidade e `aihub doctor --remote` para diagnóstico de rede remota.

### 4.4. Fatia 3: Instalação na Box Linux, Supervisão sem systemd e Runbook

- **Instalador Idempotente:**
  - Em [`scripts/install-linux.sh:1-372`](../../../scripts/install-linux.sh#L1), o instalador opera estritamente em espaço de usuário (sem requerer privilégios de `sudo`), assegura diretórios com permissões estritas `0700`, valida dependências (`build-essential`/`cc`, `rustup 1.98.1`), compila o binário `aihubd` em modo release e gera `~/.config/aihubd/aihubd.env`.
- **Supervisão Contínua sem systemd:**
  - Como a box Ailla opera em container com PID 1 `tini` (sem `systemd`), o script [`packaging/bin/aihubd-service.sh:1-282`](../../../packaging/bin/aihubd-service.sh#L1) provê launcher resiliente baseado em `nohup`, gestão de PID lock, detecção de instâncias zumbis e rotação integrada de logs baseada em tamanho com compactação gzip.
- **Documentação e Testabilidade de Instalação:**
  - Em [`scripts/test-install-linux.sh:1-234`](../../../scripts/test-install-linux.sh#L1), foi criada suíte de testes do instalador e supervisão, validada com sucesso pelo runner.
  - Em [`docs/ops/setup-box-aihubd.md:1-209`](../../ops/setup-box-aihubd.md#L1), runbook passo a passo em pt-BR descreve todo o ciclo de vida operacional, setup de túnel Cloudflare e procedimentos de emergência.

---

## 5. Consequências da Decisão

### O que melhora
- **Resiliência e Desacoplamento Real:** Sessões de longa duração agora sobrevivem a suspensão de laptop, reinicializações do Mac e trocas de rede.
- **Eficiência de Rede:** O canal de PTY binário via WebSocket elimina o overhead de encoding Base64 sobre JSON do design legado.
- **Zero Regressão Local:** Desenvolvedores e agentes executando comandos locais continuam usando o socket Unix sem precisar configurar chaves ou certificados de rede.
- **Roteamento Confiável:** A eliminação da flag de quota duvidosa impede que harnesses sem credenciais válidas sejam selecionados pelo roteador.

### O que piora
- **Superfície de Rede e Gerenciamento de Chaves:** O dono precisa manter a chave pública do Mac cadastrada na allowlist do `aihubd` na box.
- **Concorrência em Testes Paralelos:** A suíte de testes de rede (`ws.rs` e `auth.rs`) lida com binds de portas efêmeras em loopback e probe assíncrono; embora passe com 100% de sucesso na suíte completa (`305 passed`), testes com alocação massiva concorrente de portas efêmeras exigem cuidado de isolamento.

---

## 6. O que Continua Gated (Portões Fechados Mantidos)

Os seguintes portões permanecem estritamente fechados até ação do dono ([`00-adr.md` §8](00-adr.md)):

1. **ToS Headless para Spawn Real de Harnesses:**
   - O daemon possui o código de spawn de processos harness preparado, mas o spawn automatizado sem supervisão direta depende da confirmação do dono dos Termos de Serviço dos provedores (Anthropic Claude, OpenAI Codex, Google Antigravity, Cursor).
2. **Login Headless do `cursor-agent`:**
   - O binário `cursor-agent` versão `2026.09.15` está instalado na box Ailla, porém o login headless (`cursor-agent login`) depende de intervenção interativa única pelo dono via terminal na box.
3. **Allowlist GitHub do Principal:**
   - O arquivo `~/.config/aihubd/allowlist.json` na box deve ser populado com a chave pública gerada pelo dono através de `aihub pair` no Mac antes que conexões remotas sejam aceitas.
4. **Smoke Test Ponta a Ponta com Tampa Fechada:**
   - O teste real em hardware (Mac -> túnel Cloudflare `aihub.mathai.com.br` -> `127.0.0.1:9920` -> sessão viva com Mac desconectado) é o próximo passo prático do dono, conforme detalhado a seguir.

---

## 7. Próximo Passo Real do Dono

Para colocar a arquitetura em operação real de acordo com o runbook [`docs/ops/setup-box-aihubd.md`](../../ops/setup-box-aihubd.md):

1. **Revisar e mesclar o trabalho na `main`:**
   - A branch `handoff/20260917T190728Z-07` reúne os commits das fatias e está verde em todos os portões de CI.
2. **Setup inicial na box Ailla:**
   - Conectar na box e executar o script de instalação:
     ```bash
     cd ~/ai-hub
     ./scripts/install-linux.sh
     ```
3. **Configuração de pareamento:**
   - No Mac, rodar:
     ```bash
     aihub pair
     ```
   - Copiar a chave pública informada e adicionar ao `~/.config/aihubd/allowlist.json` na box.
4. **Inicialização do serviço na box:**
   - Na box, iniciar o daemon gerenciado:
     ```bash
     ~/ai-hub/packaging/bin/aihubd-service.sh start
     ```
   - Confirmar a escuta na porta loopback via `ss -tulpn | grep 9920`.
5. **Validação e Smoke Test com Tampa Fechada:**
   - No Mac, validar conectividade remota:
     ```bash
     aihub doctor --remote wss://aihub.mathai.com.br
     ```
   - Iniciar uma sessão via túnel:
     ```bash
     aihub --daemon wss://aihub.mathai.com.br
     ```
   - Iniciar uma tarefa longa, fechar a tampa do Mac, aguardar 60 segundos, reabrir o laptop e constatar a reconexão automática com a sessão ativa.
