# Roadmap — protondrive4linux

> Visão geral. Para princípios de arquitetura e decisões de contexto, ver
> `docs/architecture/ARCHITECTURE_PRINCIPLES.md`. Para o detalhe de cada milestone, ver os
> arquivos `M*.md` desta pasta. Para labels/Views/legenda de status, ver
> `GITHUB_PROJECT_SETUP.md`. Para a lista achatada de issues, ver `GITHUB_ISSUE_INVENTORY.md`.
> Para o critério de "pronto" de cada issue/milestone, ver
> `../contributing/DEFINITION_OF_DONE.md`.

## Milestones

| Milestone | Objetivo | Detalhe | GitHub Milestone? |
| --- | --- | --- | --- |
| M0 | Foundation & Architecture | [`M0-foundation.md`](M0-foundation.md) | Sim |
| M1 | Data Integrity | [`M1-data-integrity.md`](M1-data-integrity.md) | Sim |
| M2 | Supply Chain Security | [`M2-supply-chain.md`](M2-supply-chain.md) | Sim |
| M3 | Linux Compatibility | [`M3-linux-compatibility.md`](M3-linux-compatibility.md) | Sim |
| M4 | Performance | [`M4-performance.md`](M4-performance.md) | Sim |
| M5 | Desktop Experience | [`M5-desktop-experience.md`](M5-desktop-experience.md) | Sim |
| M6 | Encrypted Vault | [`M6-encrypted-vault.md`](M6-encrypted-vault.md) | **Não** — ver § Melhorias |
| M7 | Distribution | [`M7-distribution.md`](M7-distribution.md) | Sim |
| M8 | Cloud Drive Features | [`M8-cloud-features.md`](M8-cloud-features.md) | **Não** — ver § Melhorias |
| M9 | Production Release | [`M9-production-release.md`](M9-production-release.md) | Sim |

A numeração M0-M9 é mantida como inventário mesmo com M6 e M8 fora das Milestones ativas do
GitHub — evita renumerar tudo se a decisão mudar no futuro.

## Ordem de execução

```text
M0 Foundation → M1 Data Integrity → M2 Supply Chain Security → M3 Linux Compatibility
→ M4 Performance → M5 Desktop Experience → M7 Distribution → M9 Production Release
```

Trabalhe uma milestone por vez; não antecipar escopo de uma milestone posterior sem justificar
explicitamente o impacto sobre as anteriores.

## Backlog contínuo (não bloqueia nenhuma milestone)

- Sincronização seletiva por regra (extensão de arquivo, tamanho máximo).
- Internacionalização da GUI.
- Telemetria opt-in de erros (com consentimento explícito, nunca por padrão).
- Multi-conta Proton (ver também `M8-cloud-features.md` § M8-007, que descreve a arquitetura caso
  isso avance).
- Submissão a Flathub como formato de distribuição adicional (depois do Flatpak em si, `M7-001`).

## Melhorias — não prioritário agora

`M6-encrypted-vault.md` e `M8-cloud-features.md` documentam ideias com valor real, mas que ou
contradizem uma decisão de arquitetura já tomada, ou dependem de capacidade que o CLI oficial
não expõe hoje, ou têm complexidade/risco desproporcional ao estágio atual do projeto (M1/M2
ainda não fechadas). Ficam fora da sequência de execução até alguém decidir deliberadamente
puxar um item pra dentro de uma milestone real — o que exige revisitar o motivo documentado em
cada arquivo, não só "ter tempo sobrando".

- **Vault** (`M6-encrypted-vault.md`): reformulado como um gate de reautenticação/MFA + auto-lock
  no estilo OneDrive Personal Vault — não criptografia própria do fork. Ainda tem uma decisão de
  design em aberto (como proteger a cópia local em texto claro) antes de virar trabalho real.
- **Cloud Drive Features** (`M8-cloud-features.md`): Smart Sync esbarra na mesma decisão de não
  fazer FUSE por ora; Search/Version History/compartilhamento esbarram numa limitação do CLI
  oficial, não do fork.
