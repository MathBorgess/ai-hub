# Design: Transporte e Sessão Destacável (aihub Remote Split)

- **Sessão:** 01 — Transporte e sessão destacável
- **Documento:** `docs/design/remote-split/01-transporte-e-sessao.md`
- **Status:** Proposto para revisão
- **Princípio base:** `wiki/principles/explicit-handles-over-transport-sessions`

---

## 1. O que existe hoje no código e por que não sobrevive ao corte

O `aihub` foi desenhado originalmente para operar exclusivamente numa mesma máquina via socket local Unix. As premissas assumidas no código atual quebram imediatamente quando o daemon (`aihubd`) passa a rodar numa box Linux remota e a TUI (`aihub`) roda num Mac conectado por Wi-Fi ou túnel sujeito a latência, perda de pacotes e suspensão de energia.

1. **Acoplamento a `UnixStream` e autostart indiscriminado (`aihub/src/connection.rs:12-36`, `connect_or_start_daemon`):**
   - O cliente só sabe invocar `UnixStream::connect(socket_path)` (`connection.rs:13`).
   - Ao receber `ConnectionRefused` ou `NotFound`, invoca imediatamente `start_daemon_detached(socket_path)` (`connection.rs:19, 39-80`), tentando subir um `aihubd` local no Mac via `std::process::Command` (`connection.rs:77`).
   - *Por que não sobrevive:* Contra um daemon remoto numa box externa, subir um processo local é um erro conceitual e esconde falhas de rede.
2. **Framing único de 32 MiB com JSON sobre stream confiável (`aihub-core/src/codec.rs:7, 23-25, 106-145`):**
   - Framing baseado em prefixo de 4 bytes big-endian (`u32`) seguido de payload UTF-8 JSON (`DEFAULT_MAX_FRAME_LENGTH = 33_554_432`).
   - *Por que não sobrevive sem ajustes:* Toda a comunicação (controle, quota, PTY) trafega no mesmo canal sequencial. Um dump grande de PTY bloqueia o canal de controle.
3. **Handshake rígido sem negociação de capacidades (`aihub-core/src/ipc.rs:12, 88, 150`, `aihub/src/connection.rs:168-204`, `aihubd/src/lib.rs:951-973`):**
   - O handshake exige estritamente `Hello { version: 2 }` nos dois sentidos (`ipc.rs:12`, `connection.rs:185`, `lib.rs:963`). Se diferir, o daemon rejeita e encerra a conexão (`lib.rs:967-972`).
   - *Por que não sobrevive:* Impede negociação aditiva de capacidades (como multiplexação ou retomada sequenciada) sem quebrar o protocolo de ponta a ponta.
4. **PTY transmitido como Base64 embutido em JSON (`aihub-core/src/ipc.rs:18-81`, `CONTRACT.md` §1):**
   - `Base64Bytes` codifica cada fatia de PTY em Base64 padrão (`ipc.rs:53, 64`).
   - *Por que não sobrevive:* Em links remotos e túneis com throughput restrito, o overhead fixo de 33% em bytes e a CPU gasta em parse JSON/Base64 desperdiçam largura de banda e aumentam a latência de digitação.
5. **Canal ilimitado no cliente e descarte agressivo no daemon (`aihub/src/connection.rs:124`, `aihubd/src/lib.rs:280-303, 788-791, 974`):**
   - O leitor do cliente acumula mensagens em `tokio::sync::mpsc::unbounded_channel()` (`connection.rs:124`).
   - No daemon, cada cliente conectado possui um canal limitado de 128 posições (`mpsc::channel(128)`, `lib.rs:974`). Quando o buffer de envio enche (ex.: cliente em Wi-Fi lento recebendo rajada de logs), o daemon chama `c.sender.try_send(message)` (`lib.rs:301, 790`) e, em caso de erro, **remove e derruba o cliente sumariamente** (`lib.rs:281, 788-791`).
   - *Por que não sobrevive:* Um laptop em rede oscilante tem sua conexão descartada pelo daemon assim que o buffer enche, e o cliente acumula RAM sem contrapressão.
6. **Reconexão síncrona bloqueante com congelamento da UI (`aihub/src/lib.rs:304-334`):**
   - Na perda de conexão, o cliente entra em um laço de 10 tentativas síncronas com `tokio::time::sleep(500ms).await` (`lib.rs:308-309`), bloqueando a renderização da interface TUI por até 5 segundos. Se falhar, encerra a aplicação (`lib.rs:332`).
   - *Por que não sobrevive:* Se o laptop dorme por 3 horas, ao acordar o cliente tenta 10 vezes em 5 segundos antes da interface de rede subir e fecha sozinho, deixando o usuário sem terminal.
7. **Buffer de scrollback volátil sem numeração de sequência (`aihubd/src/lib.rs:780-783, 1059-1074`):**
   - O daemon mantém `s.pty.scrollback` limitado a 1 MiB via `drain(..excess)` (`lib.rs:782-783`). Ao reatar, envia o buffer bruto inteiro (`lib.rs:1071`).
   - O cliente processa o scrollback sobre seu emulador VT existente sem reset (`aihub/src/lib.rs:428`), arriscando duplicar sequências se o estado não for limpo.
8. **Identificadores de sessão sequenciais e previsíveis sem controle de acesso (`aihubd/src/lib.rs:1037-1045, 1049-1075`):**
   - O `SessionId` é gerado via `format!("{}-{}-{}", std::process::id(), timestamp, state.next_id)` (`lib.rs:1039-1045`).
   - O comando `Attach { target }` busca a sessão apenas pelo ID (`lib.rs:1055`), sem checar quem está pedindo.
   - *Por que não sobrevive:* Fere frontalmente a regra de handles explícitos: qualquer cliente que adivinhe o ID assume a sessão alheia.

---

## 2. As Oito Perguntas Respondidas com Decisão

### 1. Uma conexão ou duas? (Controle vs. Stream PTY)
- **Decisão:** **Dois canais lógicos estritamente desacoplados: Canal de Controle (RPC) e Canal de PTY (Data Stream).**
- **Justificativa:** No modelo mono-canal atual, um chunk de PTY de 1 MiB ou um envio de scrollback retém a conexão. Em um link remoto de 5 Mbps, isso impõe ~1,6 segundo de latência de fila, durante o qual `PtyResize`, `ListSessions`, comandos de emergência (`Detach`, `/switch`) ou pings de liveness ficam travados.
- **Topologia:**
  - *No caminho local (Unix socket):* Os dois canais continuam fluindo pela mesma conexão UDS para retrocompatibilidade total.
  - *No caminho remoto:* Duas conexões físicas independentes estabelecidas pelo cliente contra o endpoint do daemon (ou dois streams paralelos caso o transporte multiplexe nativamente, ver Seção 5):
    1. **Canal de Controle:** Mensagens JSON de baixa latência (Handshake, Lifecycle, Route, Quota, Resize, Pings de Liveness).
    2. **Canal de PTY:** Streaming contínuo de entrada/saída de terminal da sessão ativa, vinculado ao handle da sessão (`SessionId`) autenticado no handshake.

### 2. Retomada (Resumption) e Bytes Emitidos sem Ouvinte
- **Decisão:** **Ring-Buffer Sequenciado por Offset Global (`u64`) de 2 MiB por Sessão no Daemon com Delta Catch-Up.**
- **Mecanismo:**
  - Cada byte emitido pelo PTY do processo filho recebe um offset monotônico contínuo de 64 bits (`stream_offset: u64`), iniciando em 0.
  - O daemon armazena os dados em um ring-buffer em memória com teto fixo de **2 MiB por sessão**. Para 6 sessões abertas, a sobrecarga de memória é de exatamente 12 MiB — limite determinístico e imune a exaustão de RAM na box Linux.
  - O buffer mantém dois ponteiros: `head_offset` (menor offset ainda em memória) e `tail_offset` (próximo offset a ser gravado).
  - Quando o cliente reata via `Attach`:
    - Envia `Attach { session_id, last_seen_offset: Option<u64> }`.
    - **Caso A (Delta rápido):** Se `last_seen_offset >= head_offset`, o daemon transmite apenas a fatia `[last_seen_offset, tail_offset)`. O cliente alimenta seu parser VT incrementalmente sem reprocessar o terminal inteiro.
    - **Caso B (Buffer estourado / reconexão após horas):** Se `last_seen_offset < head_offset` (ou `None`), o daemon responde com `Attached { session_id, stream_offset: tail_offset, scrollback: full_buffer, gap_detected: true }`. O cliente limpa seu emulador VT e repinta a tela do zero com os 2 MiB mais recentes.
- **Retenção:** O scrollback dura enquanto a sessão existir. Se o processo filho terminar com o cliente desconectado, a sessão permanece em estado retido (tombstone) por um **TTL de 24 horas** para inspeção e merge antes de ser purgada.

### 3. Garantia de Entrega: Duplicar é Pior que Perder
- **Decisão:** **Garantia de entrega estritamente ordenada e deduplicada (Exactly-Once dentro da janela do buffer). Duplicar bytes é estritamente proibido.**
- **Justificativa:** Em emuladores de terminal VT100/ANSI, "at-least-once" ingênuo causa repetição de fatias de sequências de escape (ex.: `\x1b[31m`), quebrando o parser de estado do ratatui/crossterm e corrompendo a tela de forma permanente.
- **Comportamento em falha de rede:**
  - O cliente rastreia `last_seen_offset`. Bytes recebidos com offset menor ou igual ao já processado são descartados imediatamente.
  - Se a lacuna for irrecuperável (`last_seen_offset < head_offset`), o daemon declara perda explícita via flag `gap_detected: true`. O cliente executa reset completo do terminal (`\x1bc`) e recarrega o estado visual a partir do snapshot retido. Perder histórico antigo com aviso de reset visual é seguro; duplicar texto na tela é inadmissível.

### 4. Liveness e Isolamento da Sessão
- **Decisão:** **Heartbeat bidirecional no Canal de Controle a cada 5 segundos; timeout de 15 segundos declara cliente morto, mas a sessão NUNCA é terminada por queda de cliente.**
- **Mecanismo:**
  - O cliente envia `Ping { nonce: u64 }` a cada 5 segundos no canal de controle. O daemon responde `Pong { nonce: u64 }`.
  - Se 3 pings consecutivos falharem (15 segundos sem leitura) ou se o socket emitir `EOF`/`ConnectionReset`, o daemon marca a conexão como morta.
- **Onde o isolamento é imposto:**
  - No daemon (`aihubd/src/lib.rs`), o manipulador da conexão de transporte apenas remove o cliente de `state.clients` (`lib.rs:1014`).
  - **A morte da conexão de transporte NÃO emite `SIGTERM`, NÃO interrompe os `JoinSet` da sessão e NÃO altera `s.summary.active`.** Os harnesses continuam executando em background no PTY mestre. A sessão pertence ao daemon; o cliente é apenas um visor destacável.

### 5. Reconexão na TUI e Experiência do Usuário (UX)
- **Decisão:** **Máquina de reconexão assíncrona não-bloqueante na TUI com Exponential Backoff e Banner Informativo persistente.**
- **UX:**
  - O loop principal da TUI (`aihub/src/lib.rs:200-280`) nunca é bloqueado por chamadas `sleep`.
  - Ao cair o link, a TUI exibe imediatamente um banner em destaque:
    `[DESCONECTADO] Conexão com o daemon perdida. Reconectando (tentativa {n})... Sessão {id} continua ativa no servidor. Pressione Ctrl+C para sair da TUI sem encerrar o trabalho.`
  - A TUI continua aceitando comandos locais (ex.: sair da TUI sem matar a sessão remota).
- **Backoff:**
  - Intervalo inicial: 500 ms.
  - Fator de multiplicação: 1,5x com jitter pseudoaleatório de ±20%.
  - Teto máximo de espera: 15 segundos entre tentativas.
  - Tentativas: **infinitas enquanto a janela do terminal permanecer aberta**. Se o dono fechar o laptop e reabrir 3 horas depois, a TUI continua tentando e reata a sessão assim que o Wi-Fi restabelecer a rota IP.

### 6. Contrapressão (Backpressure)
- **Decisão:** **Canal limitado no cliente (bounded channel de 256 frames) e desacoplamento do PTY no daemon via ring-buffer.**
- **Mecanismo:**
  - *No cliente:* O leitor de socket usa canal limitado (`mpsc::channel(256)`). Ao encher, suspende a leitura do socket (`read().await`), ativando a janela de fluxo de transporte (TCP window) até o servidor.
  - *No daemon:* O processo do harness no PTY **nunca é travado** por lentidão do cliente. O leitor do PTY escreve incondicionalmente no ring-buffer da sessão.
  - A task de transmissão remota do cliente consome do ring-buffer de forma assíncrona. Se o cliente for lento demais e ficar mais de 2 MiB atrás de `tail_offset`, o cursor de envio do cliente é avançado forçadamente para `head_offset` e a flag `gap_detected` é enviada no próximo frame. O harness corre em velocidade nativa; o cliente remoto lento recebe o estado atualizado sem penalizar a execução.

### 7. O Autostart e a CLI
- **Decisão:** **Separação estrita entre Alvo Local e Alvo Remoto na CLI. Autostart é exclusivo do modo local.**
- **Configuração na CLI (`aihub/src/cli.rs`):**
  - Adiciona o argumento `--daemon <TARGET>` (e variável de ambiente `AIHUB_DAEMON`):
    - Se `TARGET` for um caminho Unix (`unix:/path` ou caminho de arquivo): opera em **Modo Local**. Mantém `start_daemon_detached` em caso de `NotFound`/`ConnectionRefused`. A flag `--no-autostart` desativa essa subida se desejado.
    - Se `TARGET` for um endereço de rede (`tls://host:port` ou `wss://host:port`): opera em **Modo Remoto**. O autostart local é **terminantemente desativado**. Falhas de conexão ativam o fluxo de reconexão remota da TUI.
  - Retrocompatibilidade: A flag legada `--socket <PATH>` (`cli.rs:14`) é tratada como sinônimo direto de `--daemon unix:<PATH>`.

### 8. Compatibilidade e Versionamento do Protocolo
- **Decisão:** **Bump para `PROTOCOL_VERSION = 3` com Negociação de Capacidades retrocompatível via extensões aditivas em `Hello`.**
- **Mecanismo de Handshake:**
  - O enum `ClientMessage::Hello` é estendido de forma aditiva:
    ```rust
    ClientMessage::Hello {
        version: u32, // Envia 3 (ou 2 em clientes antigos)
        #[serde(default)]
        capabilities: Vec<String>, // ex: ["pty_multiplex", "offset_resumption", "binary_pty"]
    }
    ```
  - O daemon v3 aceita clientes v2 (socket local Unix): quando `version == 2`, desativa negociação avançada e responde `Hello { version: 2 }`, operando exatamente como hoje.
  - Quando `version >= 3`, o daemon responde com as capacidades aceitas. O caminho local atual em produção não sofre alteração funcional nem quebra de gates.

---

## 3. Semântica de Retomada e Casos de Teste

### Modelo Formal de Estado no Daemon

```text
[0 ........................ head_offset ........... last_seen_offset ......... tail_offset]
|--- descartado pelo ring buffer ---|---------- dados retidos em RAM -----------|
                                    |<-------------- 2 MiB max ---------------->|
```

1. `stream_offset: u64`: contador global de bytes de saída gerados pelo PTY.
2. Ring buffer mantém intervalo `[head_offset, tail_offset)` com tamanho máximo de 2.097.152 bytes (2 MiB).
3. `ClientMessage::Attach { target: SessionTarget, last_seen_offset: Option<u64> }`.

### Casos de Teste para Verificação Automatizada

- **Teste 1: Retomada com Delta (Wi-Fi oscila por 5 segundos)**
  - *Cenário:* Sessão emite bytes 0 a 100.000. Cliente perde conexão no byte 60.000. Durante a queda, o daemon emite do byte 60.001 ao 100.000.
  - *Ação:* Cliente reconecta e envia `Attach { last_seen_offset: Some(60000) }`.
  - *Asserção:* Daemon responde `Attached { gap_detected: false, stream_offset: 100000 }` seguido imediatamente pelo delta `[60001..=100000]`. Nenhuma linha anterior a 60.000 é reenviada. A tela da TUI não pisca.
- **Teste 2: Retomada após Estouro de Buffer (Laptop dorme por 3 horas)**
  - *Cenário:* Cliente perde conexão no byte 10.000. O harness gera 10 MiB de compilação. O ring buffer descarta tudo anterior a 8.388.608 (8 MiB).
  - *Ação:* Cliente reconecta com `Attach { last_seen_offset: Some(10000) }`.
  - *Asserção:* Daemon detecta `last_seen_offset (10000) < head_offset (8388608)`. Responde com `Attached { gap_detected: true, stream_offset: tail_offset, scrollback: ring_buffer_bytes }`. Cliente recebe o evento, reseta o parser VT e renderiza o estado final limpo sem duplicatas.
- **Teste 3: Sessão Órfã sem Ouvinte**
  - *Cenário:* Cliente encerra a TUI abruptamente com `SIGKILL`.
  - *Asserção:* O daemon remove o cliente da lista de ativos em 15s. O processo do harness continua rodando no PTY. 10 minutos depois, um novo cliente dá `Attach` e recebe o log produzido na ausência de clientes.
- **Teste 4: Expiração de Sessão Concluída (TTL)**
  - *Cenário:* Sessão conclui execução (`active = false`), cliente desconecta.
  - *Asserção:* Sessão permanece consultável por 24 horas. Após 24h + 1s, o garbage collector do daemon desaloca os buffers e limpa o registro da sessão.

---

## 4. O Handle como Referência e Amarração ao Principal

Conforme formulado em `wiki/principles/explicit-handles-over-transport-sessions`:
> *"A handle is a reference, never a credential."*

### Regras de Handle e Amarração:
1. **Geração Segura (CSPRNG):**
   - O código atual em `aihubd/src/lib.rs:1039-1045` gera `SessionId` com `format!("{}-{}-{}", pid, nanos, next_id)`. Esse padrão é previsível e enumerável.
   - O `SessionId` DEVE ser gerado exclusivamente via CSPRNG de 128 bits (ex.: UUIDv4 ou token alfanumérico seguro com prefixo, ex.: `sess_01J7F...`).
2. **Amarração no Servidor (`<principal_id>:<session_id>`):**
   - O daemon associa cada sessão criada ao `principal_id` autenticado na conexão que a gerou (a identidade do principal é estabelecida na **Sessão 02** via credencial verificada).
   - O estado do daemon armazena o mapa de posse: `session.owner = principal_id`.
3. **Checagem Estrita de Rejeição:**
   - Quando qualquer cliente submete `Attach { target: SessionTarget::Id(session_id) }`, `PtyInput`, `Detach` ou `MergeRequest`, o daemon valida:
     1. A sessão existe no registro? (Se não, erro `SessionNotFound`).
     2. O `session.owner` é idêntico ao `principal_id` autenticado na conexão atual?
     3. Se diferente: a requisição é **terminantemente rejeitada** com `UnauthorizedSessionAccess` (e logada em trilha de auditoria). A mera posse do `SessionId` não confere autorização de leitura ou escrita no terminal da sessão.
4. **Ciclo de Vida de Sessões Órfãs:**
   - Desconexão de transporte não encerra sessão órfã.
   - O descarte só ocorre por:
     - Comando explícito `MergeRequest` com estratégia de merge/discard.
     - TTL de 24 horas após término natural do processo filho sem reconexão.

---

## 5. Análise de Candidatos de Transporte

Conforme diretriz, dependências não são fixadas nesta etapa; são apresentadas com seus méritos e critérios de desqualificação:

| Candidato | Prós | O que desqualifica |
|---|---|---|
| **WebSocket sobre TLS (`tokio-tungstenite`)** | Compatibilidade universal com firewalls, túneis HTTP/S existentes (porta 443) e gateways corporativos; excelente ecossistema assíncrono em Rust; framing binário nativo. | TCP subjacente sofre de Head-of-Line blocking se controle e PTY compartilharem o mesmo socket (exige abrir duas conexões WS paralelas); mudança de rede (Wi-Fi -> 4G) exige novo handshake TLS completo. |
| **QUIC nativo (`quinn`)** | Multiplexação real de streams independentes sem Head-of-Line blocking em uma única conexão; migração de conexão integrada (troca de IP sem cair a sessão); handshake TLS 1.3 rápido (0-RTT). | Bloqueio frequente de tráfego UDP em redes corporativas/cafés; túneis reversos populares (gateways HTTP) frequentemente não roteiam UDP puro sem encapsulamento. |
| **TCP cru com TLS (`tokio-rustls`)** | Overhead mínimo de protocolo; aproveita quase diretamente o framing de 4 bytes do `aihub-core/src/codec.rs`. | Exige exposição direta de porta TCP ou túnel TCP dedicado; não atravessa proxies reversos HTTP padrão; não oferece multiplexação nativa. |
| **SSH Multiplexado (`russh` / `openssh`)** | Usa infraestrutura de chaves SSH já existente na box; suporta múltiplos canais lógicos nativos (canal 0 = controle, canal 1 = PTY). | Complexidade e peso de dependência desproporcionais; dificuldade de orquestrar renovação de tokens e políticas de autorização granulares da Sessão 02 dentro do subsistema SSH. |

### Decisão sobre a Codificação do PTY: Base64 vs. Binário
- **Decisão:** No canal de controle remoto e no socket local mantemos JSON/Base64 para retrocompatibilidade. **No canal de PTY remoto dedicado, adota-se frame binário puro (`[4 bytes length][raw bytes]`).**
- **Justificativa:** Elimina 33% de tráfego redundante no túnel e zera o overhead de serialização/deserialização Base64 por tecla digitada e por linha de scrollback.

---

## 6. Alternativas Rejeitadas e seus Custos

1. **Rejeitada: Canal único serializado com multiplexação no JSON**
   - *Custo do descarte:* Exige manter duas conexões de rede ativas (Controle e PTY) no modo remoto.
   - *Motivo da rejeição:* Um envio de 1 MiB de scrollback travaria o canal de controle por mais de 1 segundo em conexões lentas, congelando eventos de resize, status e pings de liveness.
2. **Rejeitada: Semântica de entrega "At-Least-Once" sem numeração de offset**
   - *Custo do descarte:* Exige manter ring-buffer de 2 MiB sequenciado por sessão em RAM no daemon.
   - *Motivo da rejeição:* O reenvio de bytes duplicados no stream de terminal corrompe irreversivelmente o estado interno do emulador VT100/ANSI na TUI.
3. **Rejeitada: QUIC como transporte exclusivo e obrigatório**
   - *Custo do descarte:* Não usufruímos da migração transparente de conexão UDP entre Wi-Fi e celular no primeiro momento.
   - *Motivo da rejeição:* UDP é bloqueado em ambientes corporativos e em gateways reversos simples que operam apenas em HTTP/HTTPS.
4. **Rejeitada: Permitir `Attach` livre baseado apenas no conhecimento do `SessionId`**
   - *Custo do descarte:* Exige manter tabela de posse de principal no daemon e integrar com o subsistema de autenticação da Sessão 02.
   - *Motivo da rejeição:* Quebra a segurança básica do sistema ao expor sessões ativas a ataques de adivinhação ou vazamento de logs.

---

## 7. Trade-offs do Desenho

- **O que melhora:**
  - Robustez a falhas de rede: o dono pode fechar o laptop no escritório, abrir num café 3 horas depois e reatar exatamente onde estava.
  - O daemon e os agentes de IA nunca pausam nem são encerrados por oscilações no link do cliente.
  - A TUI não congela a renderização durante desconexões e fornece feedback transparente ao usuário.
  - Uso de banda no PTY reduzido em ~33% com framing binário dedicado.
- **O que piora:**
  - O daemon consome 2 MiB adicionais de RAM por sessão para o ring-buffer de retomada (~12 MiB para 6 sessões).
  - O cliente remoto gerencia duas conexões (ou dois streams lógicos) em vez de um único socket.
  - Se a desconexão exceder a janela de 2 MiB, o cliente perde os logs intermediários e sofre um reset de tela (embora o estado final continue correto).

---

## 8. O que isso quebra

1. **Contrato IPC (`docs/CONTRACT.md` §1 e `aihub-core/src/ipc.rs`):**
   - `PROTOCOL_VERSION` avança para 3.
   - `ClientMessage::Hello` ganha campo opcional `capabilities: Vec<String>`.
   - `ClientMessage::Attach` ganha campo opcional `last_seen_offset: Option<u64>`.
   - `DaemonMessage::Attached` ganha campos `stream_offset: u64` e `gap_detected: bool`.
2. **Caminho Local Atual:**
   - **NÃO quebra.** Clientes locais sem flags remotas continuam apontando para o socket Unix padrão (`default_socket_path`) e executando `start_daemon_detached` normalmente. O daemon aceita conexões locais v2 e v3.
3. **Testes Existentes:**
   - Testes de protocolo que validam `Hello { version: 2 }` direto em `aihub/tests/protocol.rs` precisam aceitar versão negociada ou continuar rodando em modo v2 legado.
4. **Instalação e CLI:**
   - O binário `aihub` ganha flags `--daemon` e `--no-autostart`, preservando `--socket` como alias retrocompatível.

---

## 9. Decisões que Precisam do Dono

1. **Transporte Primário Remoto:** Padronizar em **WebSocket sobre TLS (duas conexões)** pela compatibilidade universal com qualquer túnel reverso/proxy HTTP, ou adotar **QUIC (`quinn`)** como transporte padrão assumindo o requisito de rota UDP aberta?
2. **Dimensionamento do Ring-Buffer de Retomada:** Manter o teto em **2 MiB por sessão** (suficiente para ~30.000 linhas de log antes de truncar) ou elevar para **5 MiB por sessão** (30 MiB de RAM total para 6 sessões simultâneas)?
3. **Política de Descarte de Sessões Órfãs Concluídas:** Adotar **TTL fixo de 24 horas** após conclusão do harness sem cliente conectado, ou manter a sessão retida indefinidamente até que o usuário execute um comando explícito de limpeza (`aihub clean` / `/merge`)?
