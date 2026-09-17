# 04 — Operação do `aihubd` na box Linux

> Escopo: build, artefato, supervisão, log, exposição de rede e onde vive o código quando `aihubd` sai do Mac e passa a rodar numa box Linux remota. Protocolo/heartbeat são da sessão 01, autenticação/autorização da sessão 02, alocação de harness/credencial da sessão 03 — este documento assume as decisões delas e desenha o resto.

## 1. O que existe hoje (e por que não sobrevive ao corte)

- **Instalação é macOS-only.** `scripts/install.sh` depende de `plutil` (`scripts/install.sh:132`), grava `LaunchAgents` (`scripts/install.sh:19,433-463`) e chama `launchctl bootstrap`/`bootout` (`scripts/install.sh:460-463`). `docs/INSTALL.md:3` já declara: "Linux and systemd are **not** supported by these scripts yet; use `cargo install` manually on other platforms." Não existe `scripts/install-linux.sh`, nem `packaging/systemd/`, nem equivalente — só `packaging/launchd/` (dois `.plist`).
- **`aihubd` é um daemon em foreground, sem framework de log.** `aihubd/src/main.rs:10-27` só faz `Daemon::new(...).run(socket, shutdown_signal())`; não há `tracing`/`env_logger` em nenhum `aihubd/src/*.rs` (busca confirmada, sem ocorrência). Quem redireciona stdout/stderr para arquivo hoje é o `.plist` (`StandardOutPath`/`StandardErrorPath` em `scripts/install.sh:399-401,409-410`), não o binário.
- **`rust-toolchain.toml` fixa um canal exato**, não um mínimo: `channel = "1.98.1"` (`rust-toolchain.toml:2`). Qualquer build — na box ou cross — precisa desse toolchain instalado (`rustup toolchain install 1.98.1` mais o `target` do Linux via `rustup target add`), porque `rustup` vai honrar esse arquivo automaticamente sempre que o comando `cargo`/`rustc` rodar dentro do checkout.
- **`aihub-probe` já tem os `cfg` que isolam macOS.** `aihub-probe/Cargo.toml:18-19` só puxa `security-framework` `[target.'cfg(target_os = "macos")'.dependencies]`; o código correspondente já tem par `cfg(target_os = "macos")` / `cfg(not(target_os = "macos"))` (`aihub-probe/src/claude.rs:190,205`; `aihub-probe/src/antigravity.rs:31,350,356,461`). Ou seja: **o problema dos `cfg` do probe já está resolvido no código atual** — compilar `aihub-probe` para `x86_64/aarch64-unknown-linux-{gnu,musl}` não deveria falhar por causa do Keychain. (Isso não decide *onde* a leitura de credencial acontece daqui pra frente — essa é a sessão 03.)
- **`default_data_dir`/`default_socket_path` não são macOS-específicos.** `aihub-core/src/paths.rs:5-11` lê `$HOME` e junta `.local/share/aihub`; `aihub-core/src/paths.rs:14-21` deriva o socket daí. Em Linux, com `$HOME` setado, essas funções já devolvem um caminho sensato sem qualquer mudança de código — o que falta é tudo em volta (quem cria o diretório, quem sobe o processo, quem gira o log).
- **`aihub-git` assume um `repo_path` acessível localmente ao processo.** `create_session_worktree(repo_path, ...)` (`aihub-git/src/lib.rs:116-201`) roda `git worktree add` e `git rev-parse` diretamente em `repo_path` via `std::process::Command` (`aihub-git/src/lib.rs:40-46`). Não há nenhuma chamada `push` em `aihub-git/src/lib.rs` (busca confirmada) — hoje o crate só opera localmente: cria worktree, mede diff, faz merge/squash na branch de origem. Nunca fala com um remoto.
- **Harnesses viram processos independentes do `aihubd`.** `portable-pty` chama `setsid` no `pre_exec` do filho (`aihub-pty/src/spawn.rs:33` comentário), então cada harness lançado vira líder de um novo grupo de processos — não um filho comum que morre com o pai. Isso importa diretamente para a pergunta "o que acontece se `aihubd` cair às 4h": os processos de harness **não** morrem junto (ver §3).
- **CI só roda em `macos-latest`** (`docs/INSTALL.md`, seção "CI"; `.github/workflows/ci.yml` não verificado linha a linha, mas citado no INSTALL). Um caminho Linux não ganha cobertura de CI automaticamente.

## 2. Build: cross-compile ou compilar na box

**Inventário box Ailla (2026-09-17):** Linux amd64, user `box`, PID 1 `tini` (sem systemd), `cc`/`gcc` e `build-essential` **presentes**, `rustc`/`cargo` do sistema **1.85.0** (abaixo do pin `rust-toolchain.toml` **1.98.1**), `tmux` 3.5a, sem `sshd`/:22. Detalhe em [05-alvo-box-ailla.md](05-alvo-box-ailla.md). Com `cc` OK, o bloqueio de build passa a ser **só** instalar rustup/toolchain 1.98.1 (e espaço para `target/`). Se OOM/disco falhar, `cross` no Mac permanece fallback.

O workspace usa `rusqlite` com `features = ["bundled"]` (`Cargo.toml:57`), o que compila SQLite em C — **isso exige um compilador C para o alvo, tanto no caminho "compilar na box" (a box precisa de `cc`) quanto no caminho "cross-compile" (o Mac precisa de um cross-toolchain C, não só Rust)**. Esse é o maior custo real de qualquer dos dois caminhos, não o probe.

Alvo, se cross: **glibc por padrão** (`x86_64-unknown-linux-gnu` ou `aarch64-unknown-linux-gnu`, conforme arquitetura da box — também desconhecida), porque casa com uma distro comum e não exige linkar `libc` estática. **musl** (`*-unknown-linux-musl`) vira candidato só se a box for um ambiente minimalista (Alpine-like) ou se o binário precisar rodar sem depender da versão de glibc do host — custo: toolchain musl mais raro no Mac, e `rusqlite bundled` com musl exige `musl-gcc` disponível no cross-toolchain.

### Candidatos

| Candidato | Quando escolher | Custo de rejeitar / risco |
|---|---|---|
| **Compilar na box** (`cargo build --release`, toolchain 1.98.1 via `rustup`) | Box tem `cc`, `rustup`, disco/RAM suficientes | Desconhecido se a box aguenta; workspace inteiro (8 crates, LTO `thin` em release — `Cargo.toml:64`) pode ser pesado num container magro. Mais simples: sem imagem de cross-toolchain para manter. |
| **`cross` (Docker-based cross-compile, Mac→Linux)** | Mac já roda Docker/Colima; box não aguenta build | Adiciona Docker como dependência do fluxo de release no Mac; primeira invocação baixa imagem grande. Resolve o `cc` do `rusqlite bundled` de graça (a imagem já traz o toolchain do alvo). |
| **`cargo-zigbuild`** (zig como linker/cross-toolchain, sem Docker) | Quer cross sem Docker | Zig vira nova dependência instalada no Mac; menos testado neste workspace que `cross`; ainda depende do zig saber linkar `rusqlite bundled` para o alvo. |
| **`cargo install --git ... --target ...` direto do Mac com toolchain Linux baixado via `rustup target add`** | Nunca funciona sozinho aqui | **Rejeitado**: falta o compilador C do alvo para `rusqlite bundled`; o Mac não tem `cc` para Linux por padrão. Citado só para descartar — `rustup target add` resolve o Rust, não o C. |

**Recomendação de desenho (travada pelo inventário Ailla):** **compilar na box** como default após `rustup toolchain install 1.98.1`. `cross` fica como fallback se build nativo falhar por recurso. OK do dono ainda necessário só para *instalar* rustup (ação na máquina), não para a escolha de estratégia.

## 3. Artefato, layout e supervisão sem systemd

### O que sobe na box

Só **`aihubd`** precisa rodar na box — `aihub` (TUI) continua no Mac como cliente (é a premissa de todo o corte). O tarball de release, portanto, não precisa levar o binário `aihub`; levar os dois só se o dono quiser depurar localmente na box via SSH+terminal, o que é operação, não este desenho.

### Layout proposto (Linux)

| O quê | Caminho | Por quê |
|---|---|---|
| Binário | `~/.local/bin/aihubd` | Mesmo padrão que `LOCAL_BIN` hoje (`scripts/install.sh:13`), sem depender de nada macOS-específico. |
| Socket + dados | `~/.local/share/aihub/aihub.sock` | **Não muda**: `default_data_dir()`/`default_socket_path()` já resolvem isso sem `cfg` (`aihub-core/src/paths.rs:5-16`). |
| Log | `~/.local/share/aihub/log/aihubd.log` (mais rotação, ver abaixo) | Fica dentro do `default_data_dir()` já existente — evita inventar uma terceira raiz (o `~/Library/Logs/aihub` do macOS não faz sentido em Linux, e XDG state dir é uma opção mas exigiria um `paths.rs` novo; reaproveitar o que já existe é o caminho mais barato). |
| Config (se/quando existir) | `~/.config/aihub/config.toml` | Hoje **não existe** arquivo de config para `aihubd` (busca confirmada, sem `Config`/`config.toml` em `aihubd/src` ou `aihub-core/src`) — citado como onde iria, não como algo a construir agora. |
| Worktrees | `${TMPDIR}/aihub/worktrees/<session-id>` | **Não muda**: `default_worktree_root()` já lê `$TMPDIR` (`aihub-core/src/paths.rs:24-27`). Em Linux, `$TMPDIR` tipicamente não está setado (é convenção macOS) — cai no fallback `/tmp` (`aihub-core/src/paths.rs:26`), que é aceitável, mas **worktrees em `/tmp` num container costumam ser `tmpfs` ou some no restart** — ver §6. |

Nenhuma dessas quatro primeiras linhas exige mudança de código em `aihub-core`; a instalação em Linux só precisa de um script novo (`scripts/install-linux.sh` ou equivalente) que não toque no que já existe para macOS.

### Supervisão sem systemd

A box é um container sem systemd; o padrão que já funciona lá para outro serviço do dono é **processo longo sob `nohup` com script de lançamento, reiniciado à mão quando a box reinicia** (wiki privada, princípio já em uso — citado como padrão, não como implantação). Isso é a resposta honesta e o ponto mais fraco do desenho inteiro.

**O que "à mão" custa, concretamente:**
- `aihubd` roda em **foreground** (`aihubd/src/main.rs:10-27` não daemoniza, não faz retry) — se o processo morrer (OOM kill do container, panic, `cc` bug), não volta sozinho.
- Como os harnesses viram líderes de grupo de processo via `setsid` (`aihub-pty/src/spawn.rs:33`), eles **não morrem** quando `aihubd` cai — ficam órfãos, reparentados ao init do container, streaming PTY para ninguém, sem que o socket exista mais para o Mac reconectar e sem que ninguém os reaporte. Uma sessão "sobrevive" ao crash do daemon no sentido errado: o processo continua consumindo quota e CPU, mas fica inalcançável até alguém entrar na box e limpar.
- Sem reinício automático, uma queda às 4h fica parada até o dono notar — não há alerta, não há healthcheck externo neste desenho (fora de escopo: isso seria parte de uma exposição de rede que este documento não propõe por padrão, ver §4).

**Candidatos de supervisão (nenhum travado — trade-off para o dono escolher):**

| Candidato | Como fica vivo | Como volta no boot | Custo |
|---|---|---|---|
| **`nohup` + script de lançamento** (o padrão já em uso na box) | Não fica — cai e some | Manual | Mais simples, zero dependência nova; é a resposta honesta de hoje. Nenhuma detecção de morte. |
| **Watchdog via `cron`** (`* * * * * pgrep aihubd \|\| /path/start-aihubd.sh`) | `cron` reinicia dentro de 1 min | Ainda precisa de algo no boot chamar o script uma vez (ou o próprio `cron` reboot trigger `@reboot`, se suportado) | Depende de `cron` existir no container — **desconhecido**; muitos containers minimalistas não trazem `cron`. Reinício não é instantâneo (até 60s de janela morta). |
| **Supervisor de user-space** (`runit`, `s6-overlay`, `supervisord`) | Supervisiona de verdade: restart-on-crash, log capturado | Supervisor sobe como o processo `PID 1` do container ou via script de boot | Mais robusto, mas é **outra peça para instalar e manter** na box — o próprio motivo de não termos systemd (ambiente restrito) pode valer para essas ferramentas também; peso e disponibilidade desconhecidos até checar a box. |
| **`tmux`/`screen` persistente** | Sobrevive a desconexão SSH, não a crash do processo dentro dela | Manual | Resolve "não perder a sessão ao fechar o terminal SSH do dono", não resolve "reiniciar sozinho". Complementar, não substituto. |

**Recomendação de desenho:** manter `nohup` + script como baseline (é o que já funciona lá, zero custo de adoção), documentar o custo acima explicitamente no runbook, e apontar `supervisord`/`runit` como upgrade path quando o dono decidir que o custo do "à mão" passou a doer — sem adicionar a dependência agora. Isso é uma posição, não a decisão final: o dono escolhe se paga o custo de operar manualmente ou o custo de instalar um supervisor.

## 4. Log e rotação

Hoje **não existe rotação em lugar nenhum do repositório** (busca por `logrotate`/`rotate` em `scripts/`, `packaging/`, `docs/` sem resultado). Em macOS isso já é uma lacuna silenciosa — os `.plist` só redirecionam stdout/stderr para arquivo (`scripts/install.sh:399-401,409-410`), sem cap de tamanho. Um daemon supervisionando quatro harnesses (PTY streams inteiros, potencialmente) gera bem mais volume que o cenário atual de um processo web sidecar.

Log é onde segredo vaza por acidente: se `aihubd` ou os harnesses imprimirem token/credencial em stdout/stderr por engano, isso fica no arquivo de log indefinidamente sem rotação nem TTL.

**Candidatos:**
- **`logrotate`** (se disponível na box) apontando para o arquivo de log do script de lançamento — não exige mudar o binário Rust, só a operação. Custo: depende do pacote existir na box (desconhecido).
- **Rotação interna via `tracing-appender`** (rolling file) se/quando `aihubd` adotar `tracing` — hoje não adota (confirmado em §1), então isso implica escrever código, fora do escopo "desenho de operação" mas vale registrar como o caminho que não depende do que a box tem instalado.
- **Truncamento simples por tamanho no script de lançamento** (`> file 2>&1` com `logrotate`-lite feito à mão, ex. mover+gzip acima de N MB antes de reiniciar) — mais barato que os dois acima, mas reinventa uma ferramenta.

Sem rotação alguma seria o pior caso: disco cheio derruba o container. **Alguma forma de cap é obrigatória no desenho**, mesmo que a escolha da ferramenta fique aberta.

## 5. Exposição de rede

O transporte de hoje é **Unix Domain Socket local**: `~/.local/share/aihub/aihub.sock` (`docs/CONTRACT.md:14`, `aihub-core/src/paths.rs:14-21`) — não existe porta TCP no código atual. A sessão 01 decide o transporte remoto real (o que substitui/complementa o UDS entre Mac e box); este documento assume que **algo** vai escutar numa porta na box para a sessão 01 funcionar, e desenha só o perímetro de rede em volta disso, não o protocolo.

**Regra: o bind default é loopback (`127.0.0.1:<porta>`), nunca `0.0.0.0`.** Isso já é o padrão comprovado no `ai-memory` sidecar (`AI_MEMORY_BIND` = `127.0.0.1:49374`, `scripts/install.sh:398`) e no gateway já operando na box do dono (loopback + túnel, wiki privada). Nada autoriza exceção a esse default neste desenho: qualquer coisa além de loopback é decisão explícita do dono, documentada à parte, nunca o comportamento padrão de instalação.

**Como o Mac alcança essa porta loopback:** o padrão já validado na box do dono é **túnel nomeado apontando para a porta em loopback**, com a regra dura de que túnel e zona DNS precisam estar na mesma conta do provedor — quando não estão, a borda recusa e o sintoma não é óbvio (confirmado operacionalmente na wiki privada; aqui citado como princípio, não como implantação). Esse túnel é a fronteira que a sessão 02 protege com autenticação; este documento só garante que, sem o túnel, a porta não é alcançável de fora da box.

**Colisão de portas na box Ailla (2026-09-17):** `:8787` reports, `:9900` Hermes, `:9910` auth-broker/MCP já em uso. `aihubd` TCP: **`127.0.0.1:9920`**. Hostname CF **decidido:** **`aihub.mathai.com.br`** no named tunnel (sem Access; auth de app via GitHub) — **nunca** reusar `a2a.mathai.com.br` nem grants MCP. SSH `-L` continua opcional (sem sshd hoje). Ver [05](05-alvo-box-ailla.md) §6 e §8b.

**O que autorizaria bind além de loopback:** nada, por padrão. Um cenário hipotético — a box já estar numa rede privada confiável (ex. mesma VPC/WireGuard) sem precisar de túnel público — mudaria isso, mas é uma pergunta em aberto sobre a topologia real da box que este desenho não assume.

## 6. Onde fica o código

Esta é a pergunta que mais muda de forma com o corte.

**Hoje, `aihub-git` só entende repositório local.** `create_session_worktree(repo_path, ...)` roda `git worktree add`/`git rev-parse` diretamente em `repo_path` (`aihub-git/src/lib.rs:116-201`), e `finish_session`/`finish_validated` (`aihub-git/src/lib.rs:382,563`) fazem merge/squash **na `originating_checkout` local** — sem nenhuma chamada `push` em todo o crate (busca confirmada vazia). Se `aihubd` roda na box, **`repo_path` passa a ser um caminho na box**, e todo esse fluxo — checkout, worktree, diff, merge — acontece lá, não no Mac.

Isso significa, concretamente:

1. **O repositório vive permanentemente clonado na box.** Não faz sentido cloná-lo do zero a cada sessão (o `originating_checkout` precisa existir e estar limpo antes de `create_session_worktree` — `aihub-git/src/lib.rs:127-138` falha se `repo_path` não existir ou não for repo git válido). A box passa a ter uma cópia canônica do checkout do dono, análoga ao que `aihub-memory` já assume para o vault do `ai-memory` (bridge que lê o SQLite e a wiki via um checkout local — `docs/PLAN.md` §3.5) — o mesmo padrão se repete aqui, agora para o próprio `ai-hub`.
2. **Diff chega ao dono como texto, sobre o transporte da sessão 01.** `MergeOutcome.diff: String` (`aihub-git/src/lib.rs:36`) já é o formato transportado hoje entre `aihubd` e `aihub`; isso não muda de forma — o diff computado na box (`aihub-git/src/lib.rs:288` `diff(...)`) segue indo pro Mac como string via IPC. O que muda é a latência e o tamanho aceitável de mensagem, que é problema da sessão 01, não deste documento.
3. **Merge sai da box para o GitHub é uma capacidade nova.** Hoje o merge é 100% local (branch → `originating_checkout`, sem remoto). Para o branch mesclado chegar ao GitHub, alguém precisa dar `git push` a partir da box — e isso exige uma credencial git na box, que **não existe hoje em lugar nenhum do desenho atual**.

### Quem tem credencial git na box — candidatos

| Candidato | Como funciona | Custo de rejeitar |
|---|---|---|
| **Credencial git própria da box, escopo mínimo** (deploy key ou PAT fine-grained restrito a este repositório, push-only) | Box guarda uma credencial de máquina, separada da pessoal do dono; `aihub-git` ganharia um passo de `push` opcional após merge | Nova superfície: uma credencial de longa duração vivendo na box precisa de rotação e é um alvo se a box for comprometida. Mitigação possível (fora de escopo): se a sessão 02 evoluir um broker que emite credenciais curtas, o mesmo padrão poderia cobrir git — citado como direção, não como decisão desta sessão. |
| **Push nunca sai da box; dono puxa a branch para o Mac e empurra com a própria credencial** | Depois do `/merge` na box, o dono faz `git fetch <box> session/<id>` (via túnel/SSH da §5) no Mac e `git push` de lá, como hoje | Zero credencial nova na box, mas reintroduz um passo manual toda vez — quebra a promessa central do produto ("supervisionar sem sair do terminal"). Exige que a box exponha *algum* acesso git (SSH ou `git daemon`) além do socket do `aihubd`, ampliando a superfície de rede que a §5 tentou manter mínima. |
| **Credencial curta emitida sob demanda pelo broker da sessão 02, só no momento do push** | Estende o modelo de autorização por operação da sessão 02 para incluir "autorizar push" | Depende inteiramente do que a sessão 02 decidir existir; não posso desenhar aqui sem assumir a forma dela. Citado como a direção mais alinhada ao resto do sistema, não como algo que esta sessão fecha. |

**Recomendação de desenho:** repositório permanente na box (item 1, sem alternativa razoável dado como `aihub-git` funciona hoje) é a única peça que dá para fechar aqui. **Quem tem a credencial de push é decisão do dono** — as três opções acima têm custo real e nenhuma é obviamente certa sem saber se a sessão 02 vai ou não estender seu broker para git.

## 7. `ai-memory` na box

Hoje sobe como LaunchAgent, decisão explícita do dono de trazer junto na instalação (`scripts/install.sh:186` comentário "transitional (decisão do dono)"; `scripts/install.sh:430-463` registra os dois LaunchAgents lado a lado), com bind loopback `127.0.0.1:49374` (`scripts/install.sh:398`). O bloco inteiro está marcado para deletar quando a memória nativa do aihub existir (`scripts/install.sh:186,297` comentários "delete this block when aihub-native memory ships").

**Equivalente sem systemd:** o mesmo candidato de supervisão escolhido para `aihubd` em §3 (baseline `nohup`+script, upgrade path supervisor) serve para `ai-memory` sem inventar um segundo mecanismo — os dois processos são igualmente "long-running, reiniciado à mão" na box.

**O que muda para quem fala com ele:** nada na forma — `aihub-memory` já fala HTTP loopback com `ai-memory` (mesma topologia hoje entre dois processos no Mac). Estando os dois na mesma box, a chamada continua loopback, só que ambos os lados mudaram de máquina junto; não há novo salto de rede a desenhar aqui.

**Sem aumentar acoplamento:** este documento não propõe nenhuma mudança na relação `aihubd` ↔ `ai-memory` além de "os dois passam a rodar na box em vez do Mac, do mesmo jeito". Ele continua transitório — o mesmo comentário de remoção em `scripts/install.sh` vale para o script Linux equivalente, e o script Linux não deveria adicionar nenhuma lógica que amarre mais o `aihubd` ao `ai-memory` do que já existe hoje.

## 8. Instalação idempotente e desinstalação

O `scripts/install.sh` de hoje já dá o padrão a preservar: versão do `ai-memory` pinada (`AI_MEMORY_VERSION`, `scripts/install.sh:187`) com hash verificado antes de extrair (`ai_memory_fetch_verified`, `scripts/install.sh:222-245`, checa SHA-256 com `shasum -a 256 -c`), instalação de binário via `install -m 0755` sobrescrevendo o que já existe (idempotente por natureza, `scripts/install.sh:178-179`), e plist reescrito atomicamente a cada rodada (`install_plist_atomically`, `scripts/install.sh:363-375`, `mktemp` + `mv`).

O caminho Linux precisa preservar exatamente essas três garantias:

1. **Versão pinada + hash verificado** — se o artefato do `aihubd` Linux for distribuído como binário pré-compilado (em vez de `cargo build` na box), ele precisa do mesmo par versão+SHA-256 conferido antes de instalar, do jeito que `ai_memory_fetch_verified` já faz.
2. **Reinstalação idempotente** — rodar o instalador de novo substitui o binário e reinicia o processo supervisionado, sem duplicar processos nem deixar plist/script órfão. Isso é simples com `nohup`+script (mata o PID antigo, reescreve o script se mudou, relança); fica mais delicado se o candidato de supervisor de §3 for adotado, porque aí o instalador precisa falar a API desse supervisor em vez de gerenciar PID à mão.
3. **Desinstalação simétrica** — parar o processo, remover o script de lançamento (equivalente ao que `scripts/uninstall.sh` faz hoje com `launchctl bootout` + remoção de plist), e por padrão **manter dados** (mesma política do uninstall atual — purge é opt-in via flag, não default).

## 9. O que isso quebra

- **`docs/INSTALL.md`** hoje declara Linux como não suportado — precisa de uma seção nova (ou documento irmão), não uma reescrita da existente.
- **CI continua só macOS** — o caminho Linux fica sem cobertura automatizada até alguém adicionar um job; não é este documento que fecha isso.
- **Nenhum teste existente quebra**: `aihub-git`, `aihub-core`, `aihub-probe` já são multiplataforma no código (cfg corretos, paths baseados em env vars). O que não existe ainda é o *script* de instalação Linux — adicioná-lo não deveria tocar `scripts/install.sh`/`scripts/uninstall.sh` atuais (write-set desta sessão de qualquer forma não inclui `scripts/`).
- **Contrato IPC não muda por este documento** — ele descreve o perímetro em volta de um transporte que a sessão 01 ainda vai desenhar.

## 10. Decisões que precisam do dono

Fechadas / reduzidas pelo inventário Ailla ([05](05-alvo-box-ailla.md)):
- Capacidade mínima para build: `cc` OK; falta rustup 1.98.1 → default = compilar na box.
- Rede: só loopback + túnel (ou SSH futuro); sem rede privada tipo Tailscale hoje.
- Supervisão baseline = `nohup` + script (já é o padrão da box).

Ainda abertas:
1. **OK para instalar rustup 1.98.1** (e medir se `cargo build --release -p aihubd` cabe em disco/RAM).
2. **Quem tem credencial git de push** (§6) — fase 1 recomenda push via Mac.
3. **Upgrade de supervisão** (`supervisord`/`runit`) agora vs depois.
4. ~~Caminho remoto~~ — **fechado:** `aihub.mathai.com.br` (CF). sshd/Tailscale ficam opcionais.
5. **Rotação de log** (`logrotate` vs truncamento no script) — checar pacote na install.
