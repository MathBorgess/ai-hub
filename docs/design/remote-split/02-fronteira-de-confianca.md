# 02 — A fronteira de confiança de um daemon que executa código

## Por que isto importa mais do que parece

O `aihubd` expõe quatro classes de operação por IPC: `NewSession` (cria worktree git e lança um binário), `PtyInput` (bytes crus para dentro de um shell interativo vivo), `MergeRequest` (finaliza git e apaga worktree) e `SwitchHarness` (troca de harness em execução). O `ClientMessage` completo está em `aihub-core/src/ipc.rs:86-143`. Isto não é uma API de leitura com efeitos colaterais — é **execução remota de código com um terminal interativo anexado**. Quando o transporte é um socket Unix, a defesa é a permissão do arquivo (`0o600`, ver `aihubd/src/lib.rs:238`) mais o fato de que só quem já tem shell na máquina alcança o socket. No instante em que isso vira serviço de rede — mesmo atrás de um túnel — uma conexão não autenticada equivale a comprometimento total da box remota.

## O que existe hoje

- **O handshake não autentica.** `ClientMessage::Hello { version: u32 }` (`aihub-core/src/ipc.rs:87-88`) carrega só um número de versão de protocolo. `perform_handshake` em `aihub/src/connection.rs:168-204` envia `Hello`, espera `Hello` de volta, e falha só se a versão não bater ou o daemon responder `Error`. Não há campo de identidade, chave, assinatura ou token em `ClientMessage`, `DaemonMessage` nem em `IpcMessage` (`aihub-core/src/ipc.rs:206-224`). Depois do handshake, **qualquer** stream aceito pode emitir qualquer `ClientMessage`, inclusive `MergeRequest` e `PtyInput`.
- **O bind é sobre arquivo, não sobre identidade.** `bind()` em `aihubd/src/lib.rs:166-254` cria o diretório do socket com `0o700`, o arquivo de lock com `0o600`, o socket com `0o600`, e faz uma checagem cuidadosa de staleness (três tentativas de connect, `aihubd/src/lib.rs:205-225`) antes de assumir que pode reciclar um socket antigo. Isso é controle de acesso ao **arquivo do socket**, correto para o caso local, e não tem nenhuma relação com quem está do outro lado da conexão.
- **O accept loop não filtra.** `listener.accept()` em `aihubd/src/lib.rs:833` aceita todo stream que chega e imediatamente spawna uma task `daemon.client(stream)` sem nenhuma etapa intermediária de identidade.
- **Não existe auditoria de segurança hoje — só log de ciclo de vida.** `log_lifecycle()` (`aihubd/src/lib.rs:36-89`) escreve `eprintln!` com timestamp, nível e evento (sessão criada, parada, switch, erro de handoff). Não persiste em arquivo, não sobrevive a um restart sem redireção externa de stderr, e nenhum dos eventos hoje é "conexão recebida", "handshake aceito/rejeitado" ou "IP de origem" — porque não há de onde tirar essa informação. O único armazenamento durável adjacente é o spool de handoff do `aihub-memory` (mencionado em `aihubd/src/lib.rs:610-617`), que persiste briefs de troca de harness para continuidade de contexto, não eventos de segurança. **Correção ao brief desta sessão:** não há hoje uma trilha de auditoria de segurança durável no daemon; o que existe é log de ciclo de vida efêmero mais um spool de handoff não relacionado a autenticação. O desenho abaixo propõe o que precisa passar a existir.
- **O limite de frame de hoje é dimensionado para confiança local.** `DEFAULT_MAX_FRAME_LENGTH = 32 * 1024 * 1024` (`aihub-core/src/codec.rs:7`) existe para acomodar chunks de PTY de >1 MiB, não como defesa contra um remetente hostil — não há limite de conexões simultâneas nem de taxa em lugar nenhum do accept loop.

Nada disso sobrevive ao corte: o dia em que `aihubd` escuta em uma interface de rede (mesmo `127.0.0.1` atrás de um túnel — desenho da **sessão 04**), o handshake atual e o accept loop atual autorizam qualquer processo que alcance a porta a criar sessões, escrever no terminal delas e apagar worktrees.

## Modelo de ameaça

**Atacante:** qualquer processo capaz de abrir uma conexão TCP até o ponto de entrada do daemon remoto — não precisa de shell na box, só de alcance de rede até a porta (ou até o túnel, se ele não autentica por si — ver Pergunta 3). Inclui: alguém que descobre o hostname/porta por scan ou vazamento, um serviço comprometido na mesma rede da box, um túnel mal configurado que expõe além do pretendido.

**O que ele alcança hoje, se a porta abrir sem o desenho desta sessão:**
- Lista todas as sessões (`ListSessions`) e lê metadados de todo repositório em uso.
- Cria sessões novas, lançando qualquer harness configurado (`NewSession`) — execução de processo arbitrário na box do dono.
- Escreve bytes crus em qualquer PTY vivo (`PtyInput`) — controle de shell interativo, equivalente a acesso de terminal completo às sessões do dono.
- Força merge ou descarte de worktrees (`MergeRequest`) — pode apagar trabalho em progresso ou promover uma branch manipulada.
- Troca o harness ativo (`SwitchHarness`), possivelmente citando um handoff que carrega dados de outra sessão.
- Nenhuma dessas ações deixa rastro de origem, porque nada identifica o chamador.

**O que ele alcança depois do desenho abaixo, mesmo alcançando a porta:** nada, sem antes provar posse de uma chave aprovada pelo dono. Uma vez com a credencial, alcança exatamente o conjunto de operações que o nível dela autoriza (Pergunta 4), por um TTL curto, e cada ação fica atribuída a essa identidade no log de auditoria.

## Os padrões da wiki e o que adoto

`wiki/principles/owner-approved-agent-pairing-broker` resolve exatamente o problema de "um novo cliente pede admissão sem o dono distribuir mais um segredo duradouro à mão": descobrir → provar posse de chave → aprovação do dono via IdP → credencial curta amarrada à chave. Adoto o formato do protocolo (proof-of-possession, aprovação via IdP do dono, credencial curta) e o formato do envelope por chamada (identidade, audiência, timestamp, nonce de uso único, hash do corpo). **Não adoto** a topologia de broker separado como componente novo de rede — ver Pergunta 0 abaixo. Para o aihub, o daemon já é o único destino de todas as operações sensíveis; inserir um broker distinto na frente dele adiciona um salto de rede e um componente a proteger sem ganhar nada, porque não há múltiplos serviços downstream distintos para o broker mediar. O que sobrevive da ideia de broker é a **função**, não a topologia: alguém precisa validar a identidade e aprovar a admissão antes que o daemon aceite tráfego — isso pode ser o próprio processo `aihubd` fazendo a checagem, sem um segundo serviço.

`wiki/principles/terminate-and-reissue-credentials-at-every-trust-boundary` resolve a Pergunta 2 e a Pergunta 3: o daemon é a audiência, e ele mesmo verifica isso — nunca aceita "a assinatura bate" como suficiente. Adoto integralmente: verificação de audiência é positiva (nomeia o daemon, não apenas "veio de alguém confiável"), e nada que o daemon recebe do cliente é encaminhado adiante sem reemissão própria — isso inclui as credenciais de harness que o daemon guarda (Pergunta 8).

## Pergunta 0 — reaproveitar a fronteira do dono, ou ter a própria?

**Decisão: o aihub tem a própria fronteira de autenticação, mas o passo de aprovação (Pergunta 1, item 3) é feito através do IdP que o dono já opera — não um IdP novo.**

Custo de reaproveitar inteiramente um sistema de autenticação existente do dono (por exemplo, aceitar como válida para o aihub uma credencial emitida para outro serviço dele): viola diretamente `terminate-and-reissue-credentials-at-every-trust-boundary` — "aceitar uma credencial emitida para outro serviço porque é do mesmo emissor e a pessoa é a mesma" é listado lá como anti-padrão nomeado. Um bug ou uma mudança de escopo nesse outro sistema vira uma brecha no aihub sem o aihub nunca ter sido tocado.

Custo de ter a própria fronteira: mais um componente com segredo para guardar, rotacionar e revogar — a chave de assinatura do daemon (ou do verificador de credencial) e a lista de chaves públicas aprovadas.

A escolha: **audiência própria, aprovação via IdP existente.** O aihub emite e valida suas próprias credenciais, nomeadas para si (audiência = "aihubd"), mas o gate de aprovação de um novo par cliente-chave passa pelo login do dono no IdP que ele já usa — não um segredo novo por cliente, e não uma senha do aihub. Isso é exatamente o desenho do broker da wiki aplicado sem o salto de rede extra: o próprio `aihubd`, ao receber um pedido de pareamento, redireciona a etapa de aprovação para o IdP do dono e só então emite sua própria credencial de curta duração.

**O que me faria trocar de ideia:** se o aihub crescer para múltiplos serviços de rede distintos (não só `aihubd`) que precisem da mesma política de admissão, um broker separado passa a valer o salto extra porque aí ele efetivamente medeia N audiências, não uma.

## As nove perguntas

### 1. Identidade do cliente
**Decisão:** chave assimétrica gerada localmente no Mac, prova de posse por assinatura de desafio — não certificado de cliente (mTLS) como mecanismo primário, e não bearer de longa duração.

Justificativa: mTLS acopla identidade à camada de transporte que é desenho da sessão 01 (que ainda pode escolher TLS terminado em ponto diferente, ou um túnel que já autentica de outro jeito); proof-of-possession na camada de aplicação funciona independente de como o transporte é resolvido, e é o padrão que `owner-approved-agent-pairing-broker` documenta. Um bearer estático reintroduziria exatamente o problema que o princípio da wiki resolve: revogar um cliente revoga todos, e emitir um cliente novo significa distribuir mais um segredo duradouro à mão.

**Primeira vez na máquina, sem virar segredo colado à mão:** o Mac gera o par de chaves localmente (a privada nunca sai da máquina); o pedido de pareamento carrega só a chave pública. A etapa que concede autoridade é a aprovação do dono via IdP (Pergunta 0) — não uma cópia de token que alguém copia e cola. O que "chega à máquina" na primeira vez não é um segredo, é a URL do endpoint de pareamento, que pode ser pública (a resposta a uma tentativa de conexão sem credencial, ou um valor fixo de configuração) porque sozinha ela não concede nada.

### 2. Verificação positiva de audiência
A checagem que rejeita uma credencial bem assinada emitida para outro serviço: o daemon valida a assinatura **e** compara o campo de audiência da credencial contra sua própria identidade conhecida (um nome fixo de configuração, não algo derivado da requisição) — rejeita se a audiência estiver ausente, for genérica, ou nomear qualquer coisa diferente de "aihubd". A assinatura válida sozinha nunca é suficiente; ela só prova que o emissor assinou *alguma* credencial, não que assinou *esta* para *este* daemon. É exatamente o teste que `terminate-and-reissue-credentials-at-every-trust-boundary` chama de "o check mais provável de parecer correto e não aplicar nada" quando feito errado (checar assinatura e expiração, mas não audiência).

### 3. A borda não basta
**Concordo com a wiki: sim, o daemon valida por conta própria, mesmo que o túnel já termine TLS e autentique.** O túnel garante propriedades de transporte (confidencialidade, e possivelmente identidade de rede); não garante que o handshake de aplicação do aihub tenha rodado, nem que o chamador não tenha alcançado a porta por outro caminho de rede que contorna o túnel (uma porta exposta por engano, uma regra de firewall que muda). A verificação de autenticação fica na camada de aplicação do daemon, não delegada ao transporte — a Pergunta 5 declara exatamente que propriedade de transporte, se alguma, o desenho de autenticação pode assumir da sessão 01, mas a checagem de identidade do chamador nunca é assumida do transporte.

### 4. Autorização por operação
`ListSessions` e `MergeRequest` não custam a mesma prova. Proponho três níveis, mapeados sobre o `ClientMessage` existente (`aihub-core/src/ipc.rs:86-143`):

| Nível | Operações | Prova exigida |
|---|---|---|
| **Leitura** | `ListSessions`, `RequestQuota` | Credencial válida, audiência correta. Sem escopo adicional. |
| **Operação de sessão** | `NewSession`, `Attach`, `Detach`, `PtyInput`, `PtyResize`, `RouteRequest`, `SetMode`, `SubmitTask`, `SwitchHarness` | Credencial válida com escopo "operar sessões". |
| **Destrutiva** | `MergeRequest` | Credencial válida com escopo "operar sessões" **e** confirmação explícita por chamada — o contrato de `MergeRequest` já exige repetir a requisição para confirmar (`aihubd/src/lib.rs:269`, comentário "Contract has no separate preview message: repeat a matching request to confirm"); a segunda chamada precisa levar um envelope assinado próprio, não reaproveitar a assinatura da primeira, para que um replay da primeira não baste. |

`PtyInput` cai em **operação de sessão**, não em um nível próprio acima disso — ele já está preso a um `session_id` que só existe porque uma prova de nível "operação de sessão" o criou ou se anexou a ele via `Attach`. Ele não é mais perigoso do que `NewSession` em termos de prova exigida; é mais perigoso em **volume e continuidade** (é um stream, não uma chamada única), então a defesa dele não é um nível de autorização mais alto, é a Pergunta 5 (frescor por conexão, não por frame) e a Pergunta 8 (o que ele nunca pode carregar de volta).

Um cliente com escopo só de leitura que envia `NewSession` recebe rejeição — código de erro genérico de autorização, não um que revele qual escopo faltou (ver Pergunta 6).

### 5. Replay e frescor
Cada conexão autentica uma vez no handshake (substituindo o `Hello` atual por uma variante que carrega a credencial), e a sessão de conexão resultante é o que fica "fresca" — não é necessário assinar e checar nonce por frame de `PtyInput`, porque isso inviabilizaria a taxa de um shell interativo. A prova de frescor por-requisição (nonce de uso único + janela de timestamp, como no princípio da wiki) se aplica às operações de nível **destrutiva** (Pergunta 4) e à própria admissão de credencial nova — não ao stream de PTY depois que a conexão já autenticou.

**Dependência declarada, não fechada aqui:** isso assume que a conexão autenticada no handshake é a mesma conexão usada para todo o resto do IPC — ou seja, que o transporte não permite a um atacante injetar frames no meio de uma conexão já autenticada nem retomá-la sem re-handshake. Essa garantia (framing íntegro, retomada explícita e não silenciosa) é desenho da **sessão 01**; este documento consome essa propriedade e não a define. Se a sessão 01 permitir retomada de uma sessão de transporte sem repetir o handshake de aplicação, essa retomada precisa reautenticar — declaro isso como requisito cruzado, não como algo que decido aqui.

### 6. Falha fechada, sem oráculo
Um frame não autenticado (handshake ausente, assinatura inválida, audiência errada, credencial expirada, escopo insuficiente) recebe a mesma resposta em todos os casos: um único código de erro genérico (algo como `unauthorized`), sem distinguir "usuário não existe" de "assinatura errada" de "expirou" de "escopo insuficiente" — distinguir esses casos na resposta é um oráculo que deixa um atacante mapear credenciais válidas por tentativa e erro. O tempo de resposta não pode vazar a mesma informação: a validação de assinatura e a checagem de audiência/escopo devem rodar em tempo constante o suficiente para que "credencial de outra audiência" e "assinatura inválida" não sejam distinguíveis por latência — isso é um requisito de implementação, não uma escolha de política, e o crate escolhido para assinatura/verificação precisa ser avaliado por isso.

**O que ele registra:** o evento vai para o log de auditoria (a extensão que este documento propõe para `log_lifecycle`, ou um mecanismo durável equivalente — ver "O que isso quebra") com timestamp, motivo interno completo (para o operador, nunca para o chamador), e um identificador de conexão — nunca a credencial recebida em texto claro, só um hash dela. A conexão é fechada após a rejeição, sem manter o socket aberto esperando nova tentativa sem novo handshake completo.

### 7. Ciclo de vida e revogação
**TTL:** credencial de sessão curta (minutos a poucas horas, não dias) — renovação exige repetir prova de posse de chave contra o daemon, sem precisar de nova aprovação do dono a cada renovação, só a chave já aprovada continuar sendo a mesma. **Revogação:** a lista de chaves públicas aprovadas é o que o daemon consulta a cada renovação (não só na emissão inicial) — revogar é remover a chave dessa lista, e a próxima tentativa de renovação (no máximo um TTL de atraso) falha. Isso não exige editar arquivo na box à mão *durante* a operação normal — a lista de chaves aprovadas é o que a etapa de aprovação via IdP (Pergunta 0/1) escreve; revogar às 3h da manhã com o laptop roubado é: o dono autentica no IdP dele (de qualquer outro dispositivo) e revoga a aprovação daquela chave, e a próxima renovação do laptop roubado falha sem intervenção manual na box remota. **Decisão que precisa do dono:** se o canal de revogação em si (o "de qualquer outro dispositivo" acima) é o mesmo IdP usado para aprovação ou um caminho de emergência separado — este documento assume que é o mesmo, mas isso depende de como o dono já opera esse IdP, que é fora do escopo público deste repositório.

### 8. As credenciais que o daemon guarda — regra de não-encaminhamento
**Invariante verificável:** nenhuma credencial que o daemon guarda para autenticar-se nos harnesses do lado dele (quais são isso é decisão da **sessão 03**) aparece, em texto claro ou codificado, em nenhum `DaemonMessage` enviado ao cliente — nem em `Error { code, message }` (`aihub-core/src/ipc.rs:203`), nem em `PtyOutput { data }` (`aihub-core/src/ipc.rs:170-173`), nem em `MergeResult { diff, message }` (`aihub-core/src/ipc.rs:196-201`).

**Onde isso é imposto:** na fronteira entre o processo do harness e o daemon, não na fronteira entre o daemon e o cliente remoto — porque `PtyOutput` é um espelho literal de bytes de stdout do harness, e o daemon não pode filtrar semanticamente um stream binário de terminal em tempo real sem quebrar a experiência interativa. A imposição correta é **o harness nunca ter a credencial em um lugar que ele ecoa de volta ao terminal** — ela chega a ele por variável de ambiente do processo filho ou por um arquivo temporário que o harness lê e o próprio processo do harness não imprime, nunca por um argumento de linha de comando (que pode aparecer em `ps` ou em logs do próprio harness) nem por algo digitado como se fosse entrada do usuário. O `aihub-memory` já tem um módulo de redação (`aihub-memory/src/redact.rs`) usado para não vazar segredo nos handoffs entre harnesses; a regra de não-encaminhamento desta sessão declara que o mesmo cuidado se estende ao caminho `PtyOutput`/`Error`, mas o mecanismo de filtragem do fluxo de terminal, se algum for necessário além de "nunca imprimir a credencial em primeiro lugar", é decisão da sessão 03 porque ela é quem sabe qual formato de credencial cada harness usa.

**Teste que verifica o invariante:** nenhum valor presente no conjunto de credenciais de harness do daemon aparece como substring em nenhum `DaemonMessage` serializado, em um teste que roteia uma credencial de harness sintética (não uma real) por cada variante de `DaemonMessage` que pode carregar texto livre.

### 9. Limites
Quando o atacante não precisa estar na máquina, três limites que hoje não existem no accept loop (`aihubd/src/lib.rs:833`) passam a ser obrigatórios:
- **Conexões simultâneas não autenticadas:** um teto baixo (dezenas, não milhares) de conexões em estado "handshake pendente" por vez, para que abrir conexões sem completar o handshake não vire negação de serviço por exaustão de tasks. Conexões já autenticadas não contam nesse teto.
- **Rate limit de tentativas de handshake:** por origem de rede, um teto de tentativas de handshake malsucedidas por janela de tempo, com backoff — não para impedir um atacante determinado (ele pode rotacionar origem), mas para que uma varredura ingênua não vire o vetor mais barato de descoberta de credencial válida por força bruta.
- **Tamanho de frame antes da autenticação:** o limite de 32 MiB (`aihub-core/src/codec.rs:7`) foi dimensionado para acomodar chunks de PTY de uma sessão já em curso — um frame de handshake não autenticado não precisa desse teto. Proponho um teto bem menor (a ordem de poucos KiB, o suficiente para uma credencial e um envelope de assinatura) aplicado antes que o handshake complete; o teto de 32 MiB só se aplica à conexão depois de autenticada.

Nenhum desses três é dimensionado neste documento — os números concretos (quantas conexões, qual janela, qual teto de KiB) ficam como parâmetro de implementação a validar contra uso real, não como decisão travada aqui.

## Alternativas rejeitadas

- **Certificado de cliente (mTLS) como identidade primária.** Custo: acopla identidade de aplicação à decisão de transporte da sessão 01, que ainda está em aberto; troca de biblioteca de transporte vira troca de mecanismo de identidade. Fica como candidato caso a sessão 01 já decida por TLS terminado no próprio daemon — nesse caso mTLS e proof-of-possession por chave de aplicação podem coexistir, mas isso não deveria ser exigido por este documento.
- **Bearer de longa duração distribuído manualmente por cliente.** Custo: exatamente o anti-padrão que `owner-approved-agent-pairing-broker` descreve — revogar um cliente não revoga os outros só se cada um tiver segredo próprio, e cada segredo próprio distribuído à mão é mais um canal fora de banda para vazar.
- **Confiar inteiramente no túnel para autenticação (nenhuma checagem no daemon).** Custo: qualquer erro de configuração do túnel (porta exposta além do pretendido, autenticação do túnel desligada por engano numa atualização) vira daemon aberto sem nenhuma segunda linha de defesa — viola o requisito de falha fechada deste documento diretamente.
- **Broker separado como serviço de rede distinto do daemon.** Custo: salto de rede extra, componente novo a proteger, sem múltiplas audiências downstream reais para justificar mediá-las centralmente (Pergunta 0). Fica como candidato se o aihub crescer para múltiplos serviços de rede.
- **Autorização binária (autenticado = tudo liberado), sem níveis por operação.** Custo: colapsa `ListSessions` e `MergeRequest` na mesma prova, o que a Pergunta 4 do brief nomeia explicitamente como errado — uma credencial vazada de leitura vira credencial de destruição.

## Trade-offs do desenho proposto

- Ganho: uma conexão que não prova posse de chave aprovada pelo dono nunca alcança `NewSession`, `PtyInput` ou `MergeRequest` — o daemon remoto tem a mesma garantia de "só quem devia alcançar, alcança" que o socket Unix tem hoje via permissão de arquivo.
- Ganho: revogar um cliente comprometido (laptop roubado) não exige tocar a box remota à mão nem invalidar outros clientes.
- Ganho: escopo por operação significa que uma credencial de leitura vazada não vira ferramenta de destruição.
- Custo: o daemon agora guarda estado de segurança (lista de chaves aprovadas, estado de revogação) que não existia antes — mais uma coisa a persistir corretamente e a não perder num restart.
- Custo: latência adicional no handshake (verificação de assinatura, checagem de audiência) e na renovação periódica — pequena, mas real, comparado ao handshake atual de uma troca de `Hello`.
- Piora explícita: o caminho remoto fica estritamente mais complexo de depurar do que o local hoje — um erro de relógio no Mac ou na box (janela de timestamp) pode produzir uma rejeição de autenticação que parece um bug de rede.

## O que isso quebra

- **Contrato IPC:** `ClientMessage::Hello { version: u32 }` (`aihub-core/src/ipc.rs:87-88`) precisa ganhar um campo de credencial — mudança de shape aditiva se usar `#[serde(default)]` como o padrão já usado em `SwitchHarness.model` (`aihub-core/src/ipc.rs:127`), mas um cliente antigo sem credencial precisa ser rejeitado no caminho remoto (embora aceito no caminho local — ver abaixo), então a compatibilidade aditiva não é suficiente sozinha; o daemon remoto precisa de uma política que rejeite `Hello` sem credencial assim que autenticação estiver ligada.
- **Testes existentes:** `aihubd/tests/regression.rs` chama `log_lifecycle` diretamente (linhas 995-1002) e provavelmente constrói `ClientMessage::Hello` sem credencial em vários pontos de teste do caminho local — todos esses continuam válidos **apenas** se o daemon distinguir explicitamente modo local (socket Unix, sem autenticação, como hoje) de modo remoto (rede, autenticação obrigatória). Isso precisa ser uma opção explícita de arranque do daemon (por exemplo, ligada a qual tipo de listener foi aberto — Unix vs. TCP/rede), nunca um daemon que aceita ambos os caminhos na mesma escuta.
- **O caminho local:** continua exatamente como está — sem cerimônia, permissão de arquivo como defesa (`aihubd/src/lib.rs:166-254`). A distinção entre os dois modos não é um parâmetro de confiança que alguém pode baixar por engano; é uma decisão amarrada ao tipo de listener que o daemon abriu nesta execução — um daemon nunca escuta simultaneamente em Unix "modo confiança total" e em rede "modo autenticado" tratando as duas conexões da mesma forma.
- **Instalação:** a sessão 04 precisa saber que o daemon remoto exige, antes de aceitar qualquer tráfego de rede, que exista pelo menos uma chave aprovada — ou seja, o primeiro pareamento (Pergunta 1) precisa acontecer antes ou durante o setup, não depois.

## Decisões que precisam do dono

1. **Qual IdP concreto** hospeda a etapa de aprovação (Pergunta 0/1) — este documento não escolhe, só assume que um já existe e é operado pelo dono.
2. **Canal de revogação de emergência** (Pergunta 7): confirmar se é o mesmo IdP de aprovação ou um caminho separado, e se esse caminho funciona a partir de um dispositivo diferente do laptop supostamente roubado.
3. **Os números concretos de limite** (Pergunta 9): teto de conexões não autenticadas simultâneas, janela e teto do rate limit de handshake, tamanho de frame pré-autenticação — dependem de uso real que este desenho não tem como estimar.
4. **Se algum outro serviço de rede além de `aihubd`** está no horizonte próximo — determina se a decisão da Pergunta 0 (sem broker separado) continua certa ou se um componente de broker passa a valer o custo.


---

## Adenda 2026-09-17 — IdP escolhido

**IdP de pareamento Mac↔`aihubd`:** GitHub (conta do dono). A credencial emitida após prova de posse / Device Flow deve carregar `audience = "aihubd"` e **não** ser aceita pelo broker MCP/`a2a`. Allowlist do principal GitHub fica na config da box. Cloudflare Access **não** é obrigatório neste hostname (`aihub.mathai.com.br`); a fronteira de app é este IdP.
