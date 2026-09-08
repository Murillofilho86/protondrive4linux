# M8 — Cloud Drive Features

> **Não é uma Milestone ativa no GitHub.** Fica documentada aqui, deprioritizada, até alguém
> decidir deliberadamente puxá-la de volta — ver `docs/roadmap/ROADMAP.md` § Melhorias.

## O que é

Smart Sync (placeholders online-only, baixa sob demanda), edição offline com reconciliação,
busca por nome/extensão/tamanho, histórico de versão com restore, "Shared with me/by me".

## Por que não é prioridade agora

Motivo varia por item, mas todos batem no mesmo teto:

- **Smart Sync (online-only placeholders)**: é, na prática, o modelo de mount FUSE/hidratação
  sob demanda que o projeto decidiu conscientemente adiar (ver
  `docs/architecture/ARCHITECTURE_PRINCIPLES.md` — "sem FUSE/rclone no core"). Não dá pra ter
  "arquivo que baixa quando você abre" sem algum tipo de filesystem virtual por baixo — reabrir
  isso sem citar essa decisão anterior seria prometer uma feature que exige reabrir uma decisão
  de arquitetura grande.
- **Search, Version History, criação de links de compartilhamento**: o `proton-drive` CLI
  oficial não expõe esses comandos hoje (sem busca, sem histórico de versão, sem `share create`
  — só aparecem pastas já compartilhadas por terceiros na listagem). Não é uma questão de
  esforço do fork: sem o CLI oficial suportar, a feature não existe para construir em cima.
- **Offline mode com edição offline**: sobrepõe diretamente `M1-007` (Offline recovery, em
  `M1-data-integrity.md`) — o mecanismo de reconciliação já é responsabilidade do sync engine;
  "permitir create/edit/rename/move offline" já é o comportamento hoje enquanto o CLI está
  indisponível. O que falta é só o teste de M1-007, não uma feature nova.
- **Multi-conta**: não toca nenhuma das camadas do princípio de priorização — fica no backlog
  contínuo (`ROADMAP.md` § Backlog), não aqui.

## Quando revisitar

- **Smart Sync**: só depois de M0-M9 (ativas) estarem estáveis em uso real, e só como pesquisa
  de viabilidade primeiro (igual ao tratamento que Flatpak recebe em `M3-005`) — nunca como
  código direto.
- **Search / Version History / Share**: ficam bloqueados até o CLI oficial mudar; abrir issue
  nova só quando isso acontecer, citando a versão do CLI que passou a suportar.
- **Offline mode**: já coberto por M1-007 — não abrir de novo aqui.

## Referência: o design original (M8-001 a M8-007, não implementadas)

- **M8-001 Smart Sync**: estados `Online only`, `Available locally`, `Always keep`.
- **M8-002 Offline mode**: `create/edit/rename/move` offline, reconciliação ao voltar.
- **M8-003 Search**: por filename, extension, size, date, path.
- **M8-004 Recent files**: `Recent`, `Favorites`, `Offline`, `Conflicts`.
- **M8-005 File history**: `Version History` com `restore`, `download`, `compare`.
- **M8-006 Shared folders**: `Shared with me`, `Shared by me`.
- **M8-007 Multi-account**: `Account { credentials, configuration, baseline, sync state,
  vaults }`, nenhum estado pode vazar entre contas.
