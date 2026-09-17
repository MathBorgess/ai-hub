# Documentos de Desenho: aihub Remote Split

Índice dos documentos de decisão arquitetural para a separação remota entre o daemon `aihubd` e o cliente TUI `aihub`:

- [00-adr.md](00-adr.md): Síntese decisória — **GO condicional** para implementação na box Ailla; mapa de contradições; migração em 5 fatias; UDS local preservado.
- [01-transporte-e-sessao.md](01-transporte-e-sessao.md): Transporte remoto (controle vs PTY), retomada por offset, ring-buffer 2 MiB, handles amarrados ao principal.
- [02-fronteira-de-confianca.md](02-fronteira-de-confianca.md): Autenticação por prova de posse, autorização em três níveis, falha fechada, não-encaminhamento de credencial.
- [03-credenciais-e-quota.md](03-credenciais-e-quota.md): Execução na box, credenciais headless, autoridade de quota.
- [04-operacao-na-box.md](04-operacao-na-box.md): Build, layout, supervisão sem systemd, bind loopback, repositório canónico.
- [05-alvo-box-ailla.md](05-alvo-box-ailla.md): **Inventário vivo da box Ailla**, day-1 tmux vs day-2 aihubd, pré-condições reclassificadas, lacunas de desenvolvimento e pesquisa.

## Leitura rápida

1. Ler [05](05-alvo-box-ailla.md) se o alvo é a box da Ailla (runtime real).
2. Ler [00](00-adr.md) para o veredito e as fatias.
3. Day-1 operacional (sem aihubd) continua MAT-223/224 no vault: overlay + SSH + tmux.
