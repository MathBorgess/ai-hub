# 05 — Alvo de implementação: box Ailla (inventário vivo)

> Escopo: fechar o que o ADR 00 deixava “aberto” com o inventário **da box onde o daemon deve correr** (máquina Linux da Ailla / Grok Bot, user `box`). Não implementa código. Atualiza o enquadramento day-1 (tmux) vs day-2 (`aihubd`).
>
> Fontes: inventário MAT-223 (2026-09-15) + reconciliação MAT-224 + cutover broker 2026-09-16 + checagem live 2026-09-17 nesta box. Zero secrets / hostnames de contas pessoais neste documento além do já público (`*.mathai.com.br`).

## 1. Papel deste documento

O PR #3 nasceu como desenho do **Remote Split** com veredito NO-GO. O comentário do dono no PR já reconheceu: o vault (MAT-223/224) escolheu **day-1 = overlay + SSH + tmux**, e o Rust custom é **day-2+** quando houver gap (orquestração multi-CLI, policy, attach sem shell).

O **ai-hub** é exatamente esse gap: um `aihubd` com policy, quota, multi-harness e TUI destacável. Este documento:

1. Fixa a **box Ailla** como alvo de runtime do `aihubd`.
2. Substitui “capacidade indefinida” por inventário concreto.
3. Resolve no papel as contradições que o desenho já podia fechar.
4. Lista **lacunas de desenvolvimento e pesquisa** que ainda bloqueiam build/deploy.

## 2. Inventário vivo (2026-09-17)

| Item | Estado na box Ailla |
|---|---|
| OS / user | Linux; user `box` (uid 1000) |
| PID 1 | `tini` — **sem systemd** (`systemctl` ausente) |
| `sshd` / `:22` | **Ausente** (sem openssh-server; nada escuta 22) |
| `tmux` | **3.5a** presente |
| `rustc` / `cargo` (distro) | **1.85.0** (Debian tarball) — **abaixo** do pin do repo (`rust-toolchain.toml` → **1.98.1**; `rust-version = "1.88"`) |
| `build-essential` / `cc` | **Presentes** (`/usr/bin/cc`, `gcc`, pacote `build-essential`) |
| CLIs | `claude` ✓ · `codex` ✓ · `agy` ✓ · **`cursor-agent` / `agent` ausentes** |
| Hermes A2A | processo em **`0.0.0.0:9900`** (não misturar com aihubd; aihubd continua só loopback) |
| Auth broker / MCP | **`:9910`** loopback (`a2a.mathai.com.br` via named tunnel) |
| Reports serve | **`:8787`** loopback (`reports.mathai.com.br` + Access) |
| `cloudflared` | named tunnel ativo (mesma conta da zona) |
| Tailscale | **não** instalado |
| Padrão de serviço longo | `nohup` / script + restart manual (ex.: broker, reports) |

**Portas loopback já ocupadas:** 8787, 9900, 9910. O `aihubd` TCP deve escolher porta **livre** (sugestão de desenho: `127.0.0.1:9920`) — nunca colidir com a2a/reports/Hermes.

## 3. Relação com MAT-223 / MAT-224 (day-1 vs day-2)

```text
Day-1 (já decidido no vault):   Mac --(overlay/SSH)--► box:tmux ──► claude|codex|agy
Day-2 (este desenho / aihub):   Mac --(túnel/SSH-L)--► 127.0.0.1:PORT aihubd ──► PTY harnesses
                                  TUI aihub descartável; sessão sobrevive Mac off
```

| Camada | Day-1 (tmux) | Day-2 (`aihubd` neste ADR) |
|---|---|---|
| Persistência Mac-off | sessão tmux | sessão + ring-buffer + handles no daemon |
| Multi-CLI + policy + quota | manual / fora de produto | núcleo do aihub |
| Attach | `tmux attach` | `Attach { last_seen_offset }` + auth |
| Transporte | SSH + mux | loopback + túnel/SSH-L; UDS local continua |

**Decisão de produto:** implementar o Remote Split **como day-2 na box Ailla**, sem invalidar o day-1. O day-1 continua o caminho operacional mínimo até Fatia 3 estar smoke-testada.

## 4. Pré-condições do ADR — reclassificação

| # | Pré-condição original | Reclassificação | Ação |
|---|---|---|---|
| 1 | Termos de uso dos 4 fornecedores | **HITL dono / pesquisa jurídica** — não bloqueia desenho nem Fatia 0–1 em loopback | Issue de pesquisa; harnesses sobem só com OK escrito |
| 2 | Junta 01×02 (2 conexões vs 1 auth) | **FECHADA no desenho** | Adotar **channel_ticket** (Opção B do ADR §3): PTY secundário só com ticket emitido pelo canal de controle autenticado |
| 3 | Probe Antigravity = `None` fora de macOS | **Work item Fatia 0** | Fallback Linux (`~/.config/…` / env); até lá, router **não** despacha `agy` por quota probe — pode ainda spawnar se policy permitir Unknown |
| 4 | Capacidade box / toolchain C | **PARCIALMENTE FECHADA** | Box conhecida; falta `rustup` 1.98.1 + `cc`; ver §5 |

## 5. Build na box Ailla (decisão de desenho)

**Default:** compilar **nativamente na box** (não cross no Mac), alinhado a §2 de `04-operacao-na-box.md`.

Passos operacionais (implementação futura — não executar neste PR de docs):

1. Instalar `build-essential` (ou `gcc` + `libc-dev`) para `rusqlite` bundled.
2. Instalar `rustup` e toolchain **1.98.1** (honrar `rust-toolchain.toml`) — o 1.85 do sistema **não** basta.
3. `cargo build --release -p aihubd` (e crates necessários) no checkout canónico na box.
4. Instalar binário em `~/.local/bin/aihubd`; dados em `~/.local/share/aihub`.

**Alternativa documentada:** se OOM/disco falhar, `cross` no Mac → scp do binário (fallback do doc 04).

## 6. Rede / como o Mac chega ao `aihubd`

Regra invariante (docs 02/04 + MAT-223): **bind só `127.0.0.1:<porta>`**.

| Caminho | Viabilidade nesta box | Notas |
|---|---|---|
| SSH `-L` → loopback | **Bloqueado hoje** (sem sshd) | Requer OK dono para openssh-server **ou** Tailscale SSH |
| CF Tunnel Public Hostname → `127.0.0.1:9920` | **Viável** (padrão a2a/reports) | Hostname **novo** (ex. `aihub.mathai.com.br`); **nunca** reusar `a2a` / grants MCP |
| Tailscale + SSH | Ausente | Opcional MAT-224 1ª; install à parte |
| Expor `0.0.0.0` | **Proibido** | Anti-padrão |

Até haver SSH ou hostname Access, Fatias 1–2 testam-se **na própria box** (TUI local → loopback) ou via desktop Grok já na máquina.

## 7. Harnesses na box (realidade vs doc 03)

| Harness | Na PATH hoje | Implicação |
|---|---|---|
| Claude Code | sim | Probe/credenciais headless: research + login assistido |
| Codex | sim | Preferir padrões oficiais (`app-server` / ficheiros) onde couber |
| Antigravity (`agy`) | sim | Probe macOS-only → Fatia 0; CLI pode existir sem quota probe |
| Cursor Agent | **não** | Doc 03 não pode assumir 4/4; V1 na box = **3 harnesses** até install |

## 8. Lacunas de desenvolvimento (código)

Ordenadas pela migração em fatias do ADR §6:

### Fatia 0 — probes Linux
- [ ] Fallback `read_antigravity_session` / credenciais Claude sem Keychain
- [ ] Cursor auth em Linux **ou** feature-flag “cursor ausente”
- [ ] CI `cargo test` em `ubuntu-latest` (hoje macos-only)
- [ ] Decidir comportamento do router quando `QuotaStatus::Unknown`

### Fatia 1 — protocolo v3 + auth
- [ ] `Hello` negociável v3 + credencial opcional
- [ ] Prova de posse + `audience = "aihubd"`
- [ ] Aceitar sem auth só em UDS; rejeitar em TCP
- [ ] `SessionId` CSPRNG amarrado a `principal_id`
- [ ] Auditoria durável mínima (hoje só `log_lifecycle` efêmero) — pré-requisito de “falha fechada” útil

### Fatia 2 — transporte remoto
- [ ] WebSocket/TLS ou framing TCP; **channel_ticket** obrigatório no canal PTY
- [ ] Ring-buffer 2 MiB + `Attach { last_seen_offset }`
- [ ] Reconexão TUI sem bloquear 5s e matar a app
- [ ] Stream PTY binário (sair do Base64-over-JSON no caminho remoto)

### Fatia 3 — deploy box Ailla
- [ ] `scripts/install-linux.sh` + layout XDG
- [ ] Supervisão `nohup` + script; documentar órfãos `setsid`
- [ ] `sessions.json` em disco para reconciliar PIDs no restart
- [ ] Worktrees em `~/.local/share/aihub/worktrees` (não `/tmp` tmpfs)
- [ ] Porta `9920` (ou config) + hostname CF **ou** sshd
- [ ] Clone canónico do repo de trabalho na box; política de `git push` (Mac-push vs deploy key)

### Fatia 4 — quota / endurecimento
- [ ] Autoridade de quota só na box; suppress transcript heuristic em falha de probe
- [ ] Rotação de log + sanitização de PTY
- [ ] `aihub doctor --remote`

## 9. Lacunas de pesquisa (não-código / HITL)

1. **ToS** Anthropic / OpenAI / Cursor / Google para CLI em VPS headless com conta pessoal — bloqueia spawn real de harness, não o esqueleto do daemon.
2. **Credenciais headless** por CLI na box (OAuth device / ficheiros) sem copiar secrets para o vault.
3. **IdP de pareamento Mac↔aihubd** — reusar superfície existente (CF Access OTP só-dono? GitHub Device Flow do broker?) **sem** misturar audience MCP/`a2a`.
4. **Probe Antigravity em Linux** — onde a sessão realmente vive no disco após `agy` login.
5. **Capacidade de build** — medição: tempo/`target/` size de `cargo build --release -p aihubd` após rustup 1.98.1.
6. **Comparativo day-1** — escrever checklist T0–T5 tmux (MAT-223) vs DoD Fatia 3 aihubd (o que o mux não entrega: policy, quota push, multi-session handles).

## 10. DoD para declarar “implementação possível” (desenho fechado)

Este PR de docs considera o Remote Split **implementável na box Ailla** quando:

- [x] Inventário da box documentado (este ficheiro)
- [x] Contradição canal duplo resolvida por **channel_ticket** (ADR atualizado)
- [x] Porta/colisão e regra “não usar a2a” documentadas
- [x] Harnesses 3/4 (sem cursor-agent) explícitos
- [ ] Dono OK escrito em ToS (pesquisa) — gate de **spawn**, não de coding Fatia 0–2
- [ ] Issue(s) Linear abertas para Fatia 0 e Fatia 1 com DoD testável

## 11. Fora de escopo deste documento

- Implementar Rust / abrir Fatia 0 neste PR
- Instalar sshd / rustup / CF hostname sem OK do dono
- Alterar broker MCP / reports
- Invalidar day-1 tmux
