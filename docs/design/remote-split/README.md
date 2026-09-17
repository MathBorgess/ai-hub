# Documentos de Desenho: aihub Remote Split

Índice dos documentos de decisão arquitetural para a separação remota entre o daemon `aihubd` e o cliente TUI `aihub`:

- [00-adr.md](00-adr.md): Síntese decisória executiva com veredito (NO-GO), mapa de contradições, lacunas de junta e caminho de migração em 5 fatias preservando o modo local.
- [01-transporte-e-sessao.md](01-transporte-e-sessao.md): Desenho do transporte remoto desacoplado (controle vs. PTY binário), retomada resiliente por offset com ring-buffer de 2 MiB e handles seguros amarrados ao principal autenticado.
- [02-fronteira-de-confianca.md](02-fronteira-de-confianca.md): Fronteira de confiança do daemon na rede, autenticação assimétrica por prova de posse aprovada via IdP, autorização por operação em três níveis e regra de não-encaminhamento de credenciais.
- [03-credenciais-e-quota.md](03-credenciais-e-quota.md): Arquitetura de posicionamento de execução ("Tudo na Box"), fontes de credenciais headless, supressão de estimativa por transcrições e governança de quota autoritativa na box.
- [04-operacao-na-box.md](04-operacao-na-box.md): Engenharia operacional na box Linux (estratégia de build para rusqlite bundled, supervisão baseline sob nohup, bind loopback restrito com túnel e repositório canônico permanente na box).
