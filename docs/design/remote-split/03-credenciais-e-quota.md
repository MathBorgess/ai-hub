# Arquitetura Remota: Credenciais dos Harnesses e Governança de Quota

Este documento define **onde rodam os harnesses, onde residem as credenciais e como a quota é governada** no corte arquitetural que move o daemon (`aihubd`) do macOS local para uma box Linux remota, mantendo o Mac como cliente de apresentação TUI (`aihub`).

---

## 1. O Que Existe Hoje e Por Que Não Sobrevive ao Corte

Hoje, o aihub assume estrita colocalização em um único host macOS com display e chaveiro local. Três componentes centrais quebram no corte remoto:

1. **Leitura de credenciais ancorada no macOS Keychain:**
   - Em `aihub-probe/src/claude.rs:190-203`, a função `read_keychain_claude_credentials()` usa `security_framework::os::macos::keychain::SecKeychain` sob `#[cfg(target_os = "macos")]`. Em `claude.rs:205-208`, o fallback `#[cfg(not(target_os = "macos"))]` retorna incondicionalmente `None`.
   - Em `aihub-probe/src/antigravity.rs:31-39`, o bloco `#[cfg(target_os = "macos")]` no `probe()` chama `read_antigravity_session()`, que consulta o Keychain do macOS (`read_keychain_raw("gemini", "antigravity")`, linhas 350-354 e 461-467). Sob `#[cfg(not(target_os = "macos"))]` (linhas 356-359), a função devolve `None`.
   - Em `aihub-probe/Cargo.toml:18`, a dependência `security-framework = "2"` é ativada apenas sob `cfg(target_os = "macos")`.
   - Em Linux headless, a ausência de Keychain degrada a leitura de quota do Claude e do Antigravity para arquivos locais ou estimativa por transcrições.
2. **Supervisão de processos POSIX com garantias de kernel local:**
   - Em `aihub-pty/src/spawn.rs:33-36`, a estrutura `PtyInner` gerencia `pgid` e `child_pid`.
   - Em `spawn.rs:67-82`, o encerramento do harness usa `signal_process_group` invocando `libc::kill(-pgid, sig)`.
   - Em `spawn.rs:90-104`, a barreira síncrona de parada (`stop`) e a detecção de orfandade dependem de `process_group_is_extinct` checando `libc::kill(-pgid, 0)`.
   - Essa supervisão assume processo filho rodando no mesmo kernel que o daemon.
3. **Estimativa de quota por transcrições estritamente locais:**
   - Em `aihub-probe/src/transcripts.rs:371-410`, a função `estimate_provider_usage` varre arquivos `.jsonl` em diretórios de projeto locais. As transcrições existentes na box descrevem apenas o histórico da box, desconhecendo turnos executados no Mac.

---

## 2. Comparação dos Três Desenhos e Decisão Proposta

| Critério | Desenho 1: Tudo na Box (Proposto) | Desenho 2: Daemon na Box, Harnesses no Mac | Desenho 3: Dois Daemons (Dual-Master) |
| :--- | :--- | :--- | :--- |
| **Local de execução** | Daemon, harnesses e worktrees 100% Linux. Mac é terminal puro. | Daemon no Linux; harnesses e PTY rodando no Mac. | Um daemon no Linux, um daemon no Mac; cliente alterna. |
| **Supervisão PTY** | Intacta: `libc::kill(-pgid)` e barreira síncrona locais (`aihub-pty/src/spawn.rs:67-104`). | Destruída: vira RPC sobre rede; sem garantia contra processos zumbis se o Mac suspender. | Duplicada: duas pilhas PTY locais independentes. |
| **Isolamento de Worktree** | Local na box: `aihub-git` opera sobre o filesystem onde o harness edita. | Inviável sem sincronização remota de filesystem (NFS/Mutagen) ou rsync de diffs a cada comando. | Conflito de posse do repositório: branches divergentes em dois hosts. |
| **Persistência de Sessão** | Total: fechar o laptop não interrompe tarefas em andamento. | Nula: se o Mac suspender ou cair o Wi-Fi, o processo supervisionado morre ou fica órfão. | Parcial: apenas quando o usuário escolher manualmente rodar na box. |
| **Governança de Quota** | Fonte única na box; telemetria empurrada via `QuotaPush` (`aihub-core/src/ipc.rs:180`). | Quota lida no Mac, mas decisões de despacho tomadas pelo roteador na box. | Dois roteadores, duas visões de quota e risco alto de esgotamento cruzado silencioso. |

### Decisão Proposta: Desenho 1 (Tudo na Box)
Harnesses, processos PTY, worktrees Git temporárias e o daemon `aihubd` rodam na box Linux. O cliente no macOS é um visualizador Ratatui conectando-se via transporte seguro (Sessão 01/02), encaminhando stdin/redimensionamento e renderizando stdout/telemetria.

### Custo de Rejeição das Alternativas
- **Rejeição do Desenho 2:** Destrói o propósito fundamental da migração para servidor remoto. Se os harnesses continuam no Mac, fechar a tampa do laptop mata a execução. Além disso, o daemon perde o controle de processo do `aihub-pty`, inviabilizando a barreira de parada de 2s/4s (`spawn.rs:20-22`) e a garantia de que nenhum processo filho continuará escrevendo no disco durante um handoff.
- **Rejeição do Desenho 3:** Cria um sistema "split-brain" sem benefício arquitetural. Dois daemons consumindo a mesma conta externa em provedores com limites agregados (janelas de 5h/7d) geram corridas silenciosas de quota, e a sincronização de branches `session/<id>` entre Mac e box adicionaria complexidade de replicação distribuída.

### Critério para Trocar de Desenho
Apenas a constatação jurídica incontornável de que os Termos de Uso de um provedor essencial proíbem terminantemente a execução de sua CLI em servidores remotos, ou a impossibilidade técnica absoluta de realizar login headless para um harness insubstituível.

---

## 3. Login Headless nos Quatro Harnesses

Em um servidor Linux headless (sem X11/Wayland e sem navegador gráfico), cada harness lida com autenticação de forma distinta:

1. **Claude Code (`claude`):**
   - *Mecanismo:* O binário suporta autenticação OAuth baseada em terminal via device code / URL de autorização para abrir em outro navegador, retornando token inserido no prompt. Uma vez autenticado, salva credenciais em formato JSON. O código do probe em `aihub-probe/src/claude.rs:221-224` já procura por `.credentials.json` em `claude_config_dirs()` (`CLAUDE_CONFIG_DIR`, `~/.config/claude`, `~/.claude`).
   - *Status:* Comportamento do probe verificado em código; fluxo interativo do binário no Linux marcado como **não verificado nesta rodada** (proibição de executar harnesses reais per `common.md`).
2. **OpenAI Codex (`codex`):**
   - *Mecanismo:* Lê arquivos de autenticação em disco. `aihub-probe/src/codex.rs:48-69` busca `$CODEX_HOME/auth.json` ou `~/.codex/auth.json`. O login headless exibe URL para autenticação OAuth no navegador de qualquer dispositivo e recebe o callback de código no terminal.
   - *Status:* Verificado em código do probe; login CLI real marcado como **não verificado nesta rodada**.
3. **Cursor Agent (`cursor-agent`):**
   - *Mecanismo:* No Mac, o aihub lê o SQLite do aplicativo de desktop (`state.vscdb`, `cursor.rs:500-519`). Na box Linux headless sem o IDE Cursor instalado, essa base não existirá. O probe já contém fallback em `cursor_auth_config_paths()` (`cursor.rs:480-492`) buscando arquivos em `~/.config/cursor/auth.json`, `~/.cursor/auth.json`, etc., que contêm o token JWT.
   - *Status:* Verificado em código do probe; comportamento do binário `cursor-agent` em ambiente sem display marcado como **não verificado nesta rodada**.
4. **Antigravity CLI (`agy`):**
   - *Mecanismo:* No Mac, o probe depende do language server RPC local (`antigravity.rs:22-29`) ou do Keychain (`antigravity.rs:350-354`). No Linux headless, se o language server não estiver em execução em background, a leitura cloud quota falha porque `read_antigravity_session` retorna `None` (`antigravity.rs:356-359`). O binário `agy` autentica via fluxo OAuth de console Google Cloud ou leitura de credenciais locais de ambiente (`~/.config/antigravity/` ou `~/.gemini/`).
   - *Status:* Fallback ausente no Linux verificado em código; comportamento do CLI headless marcado como **não verificado nesta rodada**.

---

## 4. A Mesma Conta em Duas Máquinas: Mecanismo de Prevenção

### O Erro Silencioso de Roteamento
As janelas de 5 horas e 7 dias de provedores como Anthropic e OpenAI são computadas **por conta no backend do provedor**, não por máquina. Se o dono utilizar o Claude interativamente no Mac enquanto o aihub despacha tarefas na box sob a mesma conta:
- Ambos consomem o mesmo balde de tokens na nuvem.
- Se a box perder acesso à API de quota do fornecedor (OAuth/Vendor) e degradar para contagem de transcrições em disco (`QuotaSource::Transcript`, em `aihub-probe/src/transcripts.rs:371-410`), ela lerá apenas os arquivos locais da box.
- A box julgará falsamente que a quota está livre (`QuotaStatus::Ok`), despachando tarefas longas para um slot que o Mac já esgotou.

### Mecanismo de Controle Proposto
Para tornar o roteamento determinístico e imune a cegueiras de concorrência:
1. **Regra de Supressão de Transcrições para Decisão Global:**
   Na box remota, o `aihub-probe` **não deve emitir `QuotaSnapshot` com `estimated: true` e `QuotaStatus::Ok`** se o probe de fornecedor falhar. Em caso de ausência de leitura direta via API/Daemon RPC, o status deve ser reportado como `QuotaStatus::Unknown` com nota explícita (`"vendor probe unavailable on remote host; optimistic transcript routing disabled"`).
2. **Exclusão de Slots `Unknown` no Roteador:**
   O `aihub-router/src/lib.rs:199-200` e `select_route_candidate` já definem que slots com `QuotaStatus::Unknown` ou `QuotaStatus::Empty` nunca são escolhidos para dispatch. A supressão acima impede que um slot sem telemetria em tempo real receba tarefas às cegas.
3. **Interceptação Reativa de Esgotamento (PTY Rate-Limit Hook):**
   Caso um harness em execução na box retorne erro HTTP 429 ou mensagem padrão de quota excedida (capturada via scanner de saída em `aihub-pty`), o daemon marca imediatamente o slot correspondente como `QuotaStatus::Empty`, define um bloqueio temporário (`holds_until_s`) e emite imediatamente um `DaemonMessage::QuotaPush` para atualizar o roteador e todos os clientes conectados.
4. **Política Operacional de Conta Única / Conta Dedicada:**
   Recomenda-se ao dono definir nos parâmetros operacionais se a conta na box é dedicada ao aihub ou compartilhada com o desktop (ver Decisões do Dono).

---

## 5. Autoridade da Quota: Quem Lê, Quem Empurra, Quem Ganha

1. **Autoridade Canônica Única:**
   O daemon `aihubd` em execução na box Linux é a **única autoridade de quota** para o sistema.
2. **Direção dos Eventos:**
   - O daemon roda o loop de telemetria periódico (a cada 2 minutos ou pós-execução, conforme `docs/PLAN.md` §3.6).
   - O daemon transmite os snapshots via `DaemonMessage::QuotaPush { snapshots }` (`aihub-core/src/ipc.rs:180`).
   - O cliente Mac **nunca envia leituras locais de quota para o daemon**. O Mac é consumidor estrito da telemetria para fins de renderização de interface (statusline e comando `/quota`, `docs/PLAN.md` §3.7).
3. **Resolução de Conflitos:**
   Não há fusão de dados entre múltiplos hosts. A visão que governa o `route_outcome` (`aihub-router/src/lib.rs:201-224`) é a do processo supervisor na box. Se a box não consegue verificar a quota, o slot é retido até nova confirmação via API.

---

## 6. O Probe Multiplataforma (`aihub-probe`)

### Inventário de Diretivas `cfg(macos)` no Código Atual
- `aihub-probe/Cargo.toml:18`:
  `[target.'cfg(target_os = "macos")'.dependencies] security-framework = "2"`
- `aihub-probe/src/claude.rs:190`:
  `#[cfg(target_os = "macos")] pub fn read_keychain_claude_credentials()`
- `aihub-probe/src/claude.rs:205`:
  `#[cfg(not(target_os = "macos"))] pub fn read_keychain_claude_credentials()` (retorna `None`)
- `aihub-probe/src/antigravity.rs:31`:
  `#[cfg(target_os = "macos")]` protegendo a chamada a `read_antigravity_session` e consulta cloud
- `aihub-probe/src/antigravity.rs:350`:
  `#[cfg(target_os = "macos")] fn read_antigravity_session()`
- `aihub-probe/src/antigravity.rs:356`:
  `#[cfg(not(target_os = "macos"))] fn read_antigravity_session()` (retorna `None`)
- `aihub-probe/src/antigravity.rs:461`:
  `#[cfg(target_os = "macos")] fn read_keychain_raw(...)`

### Adaptações Necessárias para Linux
1. **Claude:** Não requer mudanças estruturais de biblioteca, pois `read_claude_credentials_first_usable` (`claude.rs:220-240`) já contempla arquivos `.credentials.json`. É necessário garantir que as permissões de arquivo no Linux sejam restritas (`chmod 600`).
2. **Antigravity:** O módulo precisa de um provedor de credenciais não-macOS. Candidatos:
   - *Candidato A (Leitura direta de arquivo):* Ler tokens de sessão em `~/.config/antigravity/` ou diretório de configuração do SDK, análogo ao que `codex.rs` e `cursor.rs` fazem. Desqualificador: se o formato de persistência do harness for proprietário/ofuscado sem API pública.
   - *Candidato B (Injeção via variáveis de ambiente):* Permitir que o daemon leia variáveis como `ANTIGRAVITY_API_KEY` ou `GEMINI_AUTH_TOKEN` diretamente do ambiente do daemon na box. Desqualificador: tokens de curta duração que exigem refresh contínuo.
   - *Candidato C (Biblioteca multiplataforma `keyring`):* Substituir `security-framework` direto por uma crate abstraída que use Secret Service API via DBus no Linux. Desqualificador: adiciona dependência pesada de DBus/libsecret em servidores headless mínimos sem daemon de sessão.
3. **Cursor:** No Linux headless, o caminho de SQLite (`cursor.rs:471-477`) falha graciosamente, mas o caminho de arquivo (`cursor_auth_config_paths()`, linhas 480-492) deve ser o ponto primário de leitura.
4. **Codex:** O módulo `codex.rs` já é 100% multiplataforma via arquivos JSON.

---

## 7. Fronteira de Credencial e Mitigação de Vazamento (Alinhamento com Sessão 02)

O princípio arquitetural adotado estabelece que credenciais terminam na fronteira de confiança e nunca são encaminhadas para clientes remotos.

### O Que o Daemon Passa a Guardar na Box Linux
1. `~/.claude/.credentials.json`: tokens OAuth do Claude Code (access e refresh tokens).
2. `~/.codex/auth.json`: tokens do OpenAI Codex (`access_token`, `refresh_token`, `account_id`).
3. `~/.config/cursor/auth.json`: token JWT de sessão do Cursor.
4. Arquivo de credenciais ou token de ambiente da sessão Antigravity.
5. `AI_MEMORY_AUTH_TOKEN`: segredo de autenticação local para o backend de memória.
6. Chave pública / segredo de autorização para o cliente aihub conectar-se ao socket/túnel do daemon.

### Mapeamento de Vazamentos Indiretos e Mitigações
- **Caminho 1: Stream de Terminal em Tempo Real (`DaemonMessage::PtyOutput`):**
  - *Risco:* Comandos de autenticação inicial (`claude login`, scripts de setup) ou falhas verbosas de harness podem imprimir tokens na saída do PTY.
  - *Mitigação:* O `aihub-pty` armazena até 256 KiB em memória (`SCROLLBACK_CAP`, `aihub-pty/src/spawn.rs:15`). A Sessão 02 deve proibir comandos de login interativo através da TUI remota padrão, reservando a configuração de credenciais para um bootstrap administrativo dedicado na box (ou canal isolado). Logs de scrollback nunca devem ser gravados em arquivos de texto sem sanitização.
- **Caminho 2: Mensagens de Diagnóstico e Erro (`QuotaSnapshot::note`):**
  - *Risco:* Erros de HTTP capturados em `aihub-probe/src/lib.rs:45-51` (`http_failure_note`) podem propagar URLs completas de requisição contendo tokens em query strings.
  - *Mitigação:* Sanitização explícita em `http_failure_note` para mascarar padrões sensíveis antes de popular o campo `note`. O contrato de `QuotaSnapshot` (`aihub-core/src/quota.rs:180-182`) já veta credenciais e deve ser assegurado em tempo de compilação/teste.
- **Caminho 3: Inspeção de Processos (`/proc/<pid>/cmdline`):**
  - *Risco:* Em `aihub-router/src/lib.rs:156-161`, a rotina `try_classify_with_cli` dispara subprocessos.
  - *Mitigação:* Nenhum segredo ou token deve ser passado como argumento de linha de comando (`argv`), apenas via stdin ou variáveis de ambiente herdadas estritamente pelo processo filho.

---

## 8. Posicionamento do `ai-memory` Sem Aumento de Acoplamento

Conforme `docs/PLAN.md` §3.5 e `aihub-memory/src/ai_memory.rs`:
- O `ai-memory` é uma **dependência transitória** destinada a ser substituída futuramente pelo dono. O corte remoto não deve aumentar o acoplamento a ele.
- **Topologia:** O serviço `ai-memory` pode continuar rodando no Mac (via LaunchAgent existente) ou ser instalado na box. Para o `aihubd`, o `ai-memory` é tratado como um serviço HTTP externo opcional acessado via `AI_MEMORY_SERVER_URL` (padrão `http://127.0.0.1:49374`, `aihub-memory/src/ai_memory.rs:27, 115`).
- **Garantia de Desacoplamento via Spooling Local:**
  - O `aihub-memory` já possui implementação de spooling em disco em `record_spooled_record_to` (`aihub-memory/src/ai_memory.rs:168-170`), gravando em `handoffs.jsonl` sob `AIHUB_DATA_DIR` sempre que o endpoint estiver inacessível ou falhar.
  - Na box Linux, caso o dono opte por não subir o serviço `ai-memory`, o daemon opera normalmente: gera os briefs de handoff (`NN.md`), persiste-os localmente nas worktrees e acumula os registros no arquivo de spool local `handoffs.jsonl`.
  - Nenhuma operação do ciclo de vida dos harnesses (execução, switch, merge) é bloqueada por falha de entrega ao `ai-memory`.

---

## 9. Termos de Uso: Questões para Decisão do Dono

A execução de CLIs de harness associadas a contas de assinatura pessoais em servidores Linux headless não é uma decisão técnica de código, mas uma avaliação jurídica e de conformidade contratual. As seguintes questões ficam formalmente submetidas à validação do dono:

1. **Anthropic (Claude Code CLI / `claude`):**
   - Os Termos de Serviço dos planos Claude Pro/Team e as políticas de uso aceitável autorizam a execução da CLI oficial em ambientes virtuais/headless sem interface humana direta no host?
   - O uso de tokens de autenticação gerados via OAuth em instâncias cloud remotas é passível de bloqueio por detecção de tráfego de datacenter ou violação de conta individual?
2. **OpenAI (Codex CLI / `codex`):**
   - As diretrizes de uso do ChatGPT / Codex cobrem chamadas automatizadas através do endpoint de uso WHAM (`chatgpt.com/backend-api/wham/usage`) a partir de IPs de servidores ou provedores de nuvem?
   - Há restrição contratual quanto à permanência contínua de credenciais de login em máquinas compartilhadas ou servidores virtuais não residenciais?
3. **Cursor / Anysphere (`cursor-agent`):**
   - O Contrato de Licença de Usuário Final do Cursor permite extrair o token de sessão (`cursorAuth/accessToken`) para utilização autônoma da CLI `cursor-agent` fora do aplicativo desktop oficial em uma máquina sem display?
   - O tráfego de telemetria e uso direcionado a `api2.cursor.sh` a partir de uma box Linux headless está coberto pela assinatura ativa?
4. **Google (Google DeepMind / Google Cloud para `antigravity` / Gemini):**
   - Os termos das ferramentas de desenvolvimento Antigravity exigem vínculo estrito com credenciais de desenvolvedor de máquina local ou permitem operação como serviço não interativo em servidores de compilação/execução remota?
   - O plano de uso pessoal requer migração para Service Accounts do Google Cloud Platform (com cobrança por token) ao rodar fora do ambiente de desenvolvimento pessoal?

---

## 10. Alternativas Rejeitadas, Trade-offs e Impactos

### Resumo de Alternativas Rejeitadas e Seus Custos
- **Alternativa A: Proxy de Credenciais (Mac envia credenciais temporárias para a box a cada conexão):**
  - *Custo:* Violação do princípio de término de credenciais na fronteira de confiança. Transfere segredos de longa duração pela rede continuamente, expõe tokens em trânsito e impede que a box continue operando se o cliente desconectar.
- **Alternativa B: Sincronização Bidirecional de Transcrições (rsync dos logs JSONL entre Mac e Box):**
  - *Custo:* Adiciona latência e fragilidade de sincronização em arquivos de log de até dezenas de megabytes (`DEFAULT_MAX_BYTES = 64MB`, `transcripts.rs:11`), sem resolver o problema de que cotas da API em nuvem são consumidas mais rapidamente do que os logs em disco registram.
- **Alternativa C: Harnesses Rodando em Contêineres Docker Isolados na Box:**
  - *Custo:* Complexidade operacional excessiva para o estágio atual (gerenciamento de volumes para worktrees Git, mapeamento de sockets de terminal, UID/GID mapping). Descartado nesta fase em favor de isolamento por usuário de sistema.

### Trade-offs Assumidos
- **Perda da conveniência do Keychain nativo:** O armazenamento no Linux recai sobre arquivos em disco (`~/.config/...`), exigindo disciplina estrita de permissões do sistema de arquivos (`0700` para diretórios, `0600` para arquivos de tokens).
- **Setup inicial mais exigente:** O provisionamento da box remota requer uma etapa prévia de login e configuração de tokens em ambiente terminal.
- **Concentração de credenciais na box:** A box torna-se um nó com alto nível de privilégio, exigindo endurecimento de acesso SSH, firewall e regras de rede (escopo da Sessão 02).

### O Que Isso Quebra no Código e Testes Atuais
1. **Contrato de Testes do `aihub-probe`:**
   - Testes unitários condicionados a `#[cfg(target_os = "macos")]` (ex: `antigravity.rs:461`) não exercitam o caminho Linux em pipelines de CI que rodam em agentes Ubuntu. É necessário criar fixtures de arquivos JSON para habilitar testes multiplataforma idênticos em Linux e macOS.
2. **Dependência de Compilação do `Cargo.toml`:**
   - A dependência condicional `security-framework` em `aihub-probe/Cargo.toml:18` já compila em Linux retornando implementações dummy, mas novos módulos de leitura de arquivos e variáveis devem ser adicionados sem quebrar a compilação cruzada.
3. **Caminho de Execução Local Monolítico:**
   - O modelo anterior onde `aihub` e `aihubd` compartilhavam o mesmo filesystem `/Users/...` e o socket em `~/.local/share/aihub/aihub.sock` precisará suportar o cliente apontando para conexão de rede remota (conforme desenhado na Sessão 01).

---

## 11. Decisões Que Precisam do Dono

As seguintes decisões são prerrogativas do proprietário do sistema e condicionam o início da implementação:

1. **Aprovação de Riscos de Termos de Uso:** Confirmar a ciência e conformidade com as regras de uso dos quatro provedores para operação de suas CLIs na box Linux remota.
2. **Estratégia de Provisionamento Inicial das Credenciais:**
   - *Opção A:* Procedimento assistido de cópia única (rsync seguro/scp) dos arquivos de credencial existentes do Mac para os diretórios equivalentes na box.
   - *Opção B:* Execução interativa única de `claude login`, `codex login` e setup do Antigravity diretamente na box Linux via sessão SSH administrativa.
3. **Política de Concorrência de Contas:**
   - *Opção A (Exclusividade):* As contas configuradas na box são dedicadas ao `aihub`, e o dono não as utiliza concorrentemente em sessões manuais pesadas no Mac.
   - *Opção B (Compartilhamento Concorrente):* As contas são compartilhadas; o aihub deve adotar postura estritamente pessimista no roteador, desativando qualquer suposição de quota não confirmada via endpoint remoto.
4. **Hospedagem do `ai-memory`:**
   - Decidir se o binário `ai-memory` será implantado na box Linux (via systemd unit) ou se o daemon operará em modo de spooling local permanente (`handoffs.jsonl`) até a conclusão de sua substituição planejada.
