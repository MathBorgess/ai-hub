# Runbook: Setup e Operação do `aihubd` na Box Linux (Ailla)

Este runbook descreve o procedimento operacional passo a passo para o dono instalar, configurar, supervisionar e operar o daemon `aihubd` na **box Ailla** (ambiente Linux amd64, usuário `box`, PID 1 = `tini`, **sem systemd**).

---

## 1. Contexto e Topologia

Na arquitetura **Remote Split (Fatia 3)**:
- O **`aihubd`** roda na box Linux executando os harnesses PTY e operando sobre worktrees locais não-voláteis.
- O **cliente TUI `aihub`** roda no Mac e conecta-se via rede.
- **Porta loopback:** `127.0.0.1:9920` (o daemon **nunca** faz bind em `0.0.0.0`).
- **Exposição externa:** Cloudflare Named Tunnel (`hermes-ailla-a2a`) com hostname público **`aihub.mathai.com.br`** apontando para `http://127.0.0.1:9920` (**já LIVE**).
- **Sem colisão de portas:** As portas `8787` (reports), `9900` (Hermes A2A) e `9910` (auth-broker/MCP) já estão em uso na box e não são tocadas.

---

## 2. Pré-requisitos na Box

Conecte-se à box via terminal (sessão do dono como usuário `box`):

### 2.1 Verificar ambiente e compilador C
```bash
# Confirme usuário e PID 1
id -u  # Esperado: 1000 (box)
ps -p 1 -o comm=  # Esperado: tini

# Confirme compilador C (necessário para rusqlite bundled)
cc --version || gcc --version
```
> Se `cc` ou `gcc` não estiverem presentes: `sudo apt-get update && sudo apt-get install -y build-essential`

### 2.2 Ativar toolchain Rust 1.98.1
O workspace requer Rust **1.98.1** (pinado em `rust-toolchain.toml`). A distro pode ter a versão 1.85 no PATH padrão; ative o toolchain gerenciado pelo `rustup`:

```bash
# Carregue o ambiente do rustup caso ainda não esteja no PATH
source "$HOME/.cargo/env"

# Verifique a versão
rustc --version
# Esperado: rustc 1.98.1 (...)
```

Se o toolchain 1.98.1 ainda não estiver instalado:
```bash
rustup toolchain install 1.98.1
rustup default 1.98.1
```

### 2.3 Verificar portas em uso
Certifique-se de que a porta `9920` está desocupada antes de iniciar o daemon:
```bash
# Deve retornar vazio
ss -H -t -l -n "sport = :9920" 2>/dev/null || lsof -iTCP:9920 -sTCP:LISTEN -P -n
```
> **Atenção:** Se houver um stub HTTP de teste respondendo em `127.0.0.1:9920` (usado para validar o túnel Cloudflare), encerre o processo do stub antes de subir o `aihubd`.

---

## 3. Clone Canônico do Repositório na Box

O `aihubd` manipula sessões criando git worktrees a partir de um repositório canônico local na box (`~/.local/share/aihub/worktrees`).

Clone o repositório canônico em uma pasta permanente de trabalho do usuário `box`:
```bash
mkdir -p "$HOME/src"
cd "$HOME/src"

# Clone canônico do ai-hub
git clone https://github.com/MathBorgess/ai-hub.git
cd ai-hub
```

---

## 4. Instalação do `aihubd` (Compilação Nativa e Layout)

Dentro do clone (`$HOME/src/ai-hub`), execute o instalador Linux:

```bash
./scripts/install-linux.sh
```

### O que o instalador faz:
1. Valida pré-requisitos (`cc`, `cargo`/`rustc 1.98.1`, porta `9920` livre).
2. Cria a árvore de diretórios XDG:
   - Binários: `~/.local/bin/aihubd` e `~/.local/bin/aihubd-service`
   - Estado e socket: `~/.local/share/aihub/aihub.sock`
   - Logs: `~/.local/share/aihub/log/aihubd.log`
   - Worktrees persistentes: `~/.local/share/aihub/worktrees/` (evita `/tmp` tmpfs em RAM)
3. Compila nativamente o `aihubd` em modo release (`cargo build --release --locked -p aihubd`).
4. Gera o arquivo de ambiente `~/.local/share/aihub/aihubd.env` com os caminhos dos harnesses detectados (`claude`, `codex`, `cursor-agent`, `agy`).
5. Instala o launcher de supervisão e inicia o serviço em background com `nohup`.

> **Dica:** Para apenas instalar os binários e scripts sem iniciar o daemon imediatamente, utilize `./scripts/install-linux.sh --no-start`.

---

## 5. Supervisão e Controle do Serviço (`aihubd-service`)

Como a box opera sem `systemd` (PID 1 `tini`), a supervisão de processo é feita pelo utilitário `aihubd-service`:

```bash
# Verificar status e tamanho do arquivo de log
~/.local/bin/aihubd-service status

# Iniciar o daemon (idempotente: não duplica processo se já estiver rodando)
~/.local/bin/aihubd-service start

# Parar o daemon (envia SIGTERM gracioso, seguido de SIGKILL se travar após 10s)
~/.local/bin/aihubd-service stop

# Reiniciar o daemon
~/.local/bin/aihubd-service restart

# Forçar rotação de log manual
~/.local/bin/aihubd-service rotate
```

### Rotação de Logs
- O script `aihubd-service` possui rotação de log embutida: ao exceder 10MB, o arquivo `aihubd.log` é rotacionado para `.1.gz` até `.5.gz` preservando o fluxo de escrita (`copytruncate`).
- Se a box possuir o pacote `logrotate`, o arquivo de configuração `~/.local/share/aihub/logrotate.conf` (gerado a partir de `packaging/logrotate/aihubd.conf`) pode ser acionado via cron de usuário:
  ```bash
  crontab -e
  # Adicione:
  0 0 * * * /usr/sbin/logrotate -s ~/.local/share/aihub/logrotate.status ~/.local/share/aihub/logrotate.conf
  ```

---

## 6. Smoke Tests e Validação de Conectividade

### 6.1 Smoke em Loopback Local
Verifique se o processo está ouvindo no socket Unix e na porta local:
```bash
# Verificar socket Unix
ls -la ~/.local/share/aihub/aihub.sock

# Verificar processo
pgrep -u box -fl aihubd

# Verificar tail dos logs
tail -n 20 ~/.local/share/aihub/log/aihubd.log
```

### 6.2 Smoke do Túnel Cloudflare (Mac -> Box)
A partir do Mac, teste a resolução e resposta através do named tunnel:
```bash
curl -i https://aihub.mathai.com.br/
```
O túnel encaminha diretamente para `127.0.0.1:9920` na box.

---

## 7. Primeiro Pareamento e Autenticação (Fatia 1 / 2)

1. No Mac, ao iniciar a conexão com o daemon remoto:
   ```bash
   aihub --daemon https://aihub.mathai.com.br
   ```
2. O handshake v3 negocia as credenciais com o IdP configurado (**GitHub Device Flow / OAuth**, com audiência estrita `audience = "aihubd"`).
3. Uma URL de confirmação e um código curto serão apresentados no terminal para aprovação pelo dono.
4. Após aprovado, o ticket de autorização é retido pelo Mac para conexões subsequentes.

---

## 8. Fluxo Git Push na Fase 1 (Opção B — Push pelo Mac)

Para manter a box segura e sem chaves privadas SSH ou Personal Access Tokens (PAT) de longa duração com permissão de escrita no GitHub:
- **Toda alteração mesclada pelo harness permanece na branch `session/<id>` no clone canônico da box.**
- O dono realiza o envio ao GitHub a partir do seu próprio Mac:
  ```bash
  # No Mac, adicione a box como remoto git temporário (ou via SSH / bundle fetch):
  git fetch <box-remote> session/<session-id>
  git push origin session/<session-id>
  ```
- A adoção de deploy key com escopo restrito de escrita direta na box é uma evolução posterior (Fase 2).

---

## 9. O que Continua Gated (Limitações Atuais)

Antes de operar sessões em produção, esteja atento aos seguintes portões e restrições:

1. **Spawn Real de Harnesses Gated por ToS:**
   - O provisionamento e execução autônoma contínua de CLIs em VPS headless aguarda validação jurídica dos Termos de Serviço dos provedores (Anthropic, OpenAI, Cursor, Google).
2. **Login Headless do `cursor-agent` Pendente:**
   - O binário `cursor-agent` está instalado na box, mas requer login manual interativo (`cursor-agent login`) antes de poder receber tarefas.
3. **Reinicio do Daemon é Destrutivo para Sessões Ativas:**
   - Como `aihubd` não utiliza socket handover em nível de kernel (systemd fd passing), reiniciar o serviço derruba o ring-buffer de 2 MiB em RAM e desconecta sessões em andamento. Não reinicie durante operações ativas.
4. **Processos Órfãos (`setsid`):**
   - Os harnesses são iniciados como líderes de grupo de processos. Em caso de encerramento abrupto (crash) do `aihubd`, os processos filhos continuam executando até que o dono realize limpeza ou encerramento manual via `pkill`.
5. **Trilha de Supervisor de User-space:**
   - Para ambientes onde reinício automático em crash for essencial, planeja-se adoção futura de supervisores leves como `supervisord` ou `runit` de usuário. O baseline atual permanece sob `nohup` + script.

---

## 10. Desinstalação e Limpeza

Caso deseje desinstalar o `aihubd` da box:

```bash
# Remove binários e desliga o daemon, mantendo logs e worktrees
./scripts/uninstall-linux.sh

# Para remoção completa incluindo dados, socket e logs:
./scripts/uninstall-linux.sh --purge
```
