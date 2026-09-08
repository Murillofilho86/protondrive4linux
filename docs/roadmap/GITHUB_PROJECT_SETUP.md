# GitHub Project Setup

Como configurar Milestones, Labels e Views no GitHub para acompanhar `docs/roadmap/`.

## Milestones

| Milestone | Objetivo | Cria no GitHub? |
| --- | --- | --- |
| M0 | Foundation & Architecture | Sim |
| M1 | Data Integrity | Sim |
| M2 | Supply Chain Security | Sim |
| M3 | Linux Compatibility | Sim |
| M4 | Performance | Sim |
| M5 | Desktop Experience | Sim |
| M6 | Encrypted Vault | **Não** — deprioritizado, ver `M6-encrypted-vault.md` |
| M7 | Distribution | Sim |
| M8 | Cloud Drive Features | **Não** — deprioritizado, ver `M8-cloud-features.md` |
| M9 | Production Release | Sim |

A numeração M0-M9 é mantida como inventário/nomenclatura de arquivo mesmo com M6 e M8 fora de
Milestones ativas — evita renumerar tudo se algum dia a decisão mudar. **Só 8 Milestones reais
são criadas no GitHub** (M0-M5, M7, M9); M6 e M8 existem só como documento.

## Labels

### Prioridade
`priority:critical` `priority:high` `priority:medium` `priority:low`

### Área
`area:sync` `area:storage` `area:security` `area:gui` `area:filesystem` `area:packaging`
`area:distribution` `area:performance` `area:testing` `area:documentation`

### Tipo
`type:feature` `type:bug` `type:refactor` `type:test` `type:security` `type:documentation`
`type:research`

### Status
`status:blocked` `status:needs-design` `status:ready` `status:in-progress` `status:review`
`status:done`

### Risco
`risk:data-loss` `risk:security` `risk:compatibility` `risk:performance`

Comando de referência (rodar uma vez, idempotente — `gh label create --force`):

```sh
for l in priority:critical priority:high priority:medium priority:low \
         area:sync area:storage area:security area:gui area:filesystem area:packaging \
         area:distribution area:performance area:testing area:documentation \
         type:feature type:bug type:refactor type:test type:security type:documentation type:research \
         status:blocked status:needs-design status:ready status:in-progress status:review status:done \
         risk:data-loss risk:security risk:compatibility risk:performance; do
  gh label create "$l" --force
done
```

## Views sugeridas do GitHub Project

- **Roadmap**: M0 → M9 (Milestones reais: M0-M5, M7, M9).
- **Current Sprint**: filtro `status:ready` OR `status:in-progress` OR `status:review`.
- **Security**: filtro `area:security`.
- **Data Integrity**: filtro `risk:data-loss`.
- **Distribution**: filtro `area:distribution` OR `area:packaging`.

## Ordem de execução

```text
M0 Foundation → M1 Data Integrity → M2 Supply Chain Security → M3 Linux Compatibility
→ M4 Performance → M5 Desktop Experience → M7 Distribution → M9 Production Release
```

Não antecipar escopo de uma milestone posterior sem justificar explicitamente o impacto sobre
as anteriores (ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md` § princípio de priorização).
Trabalhe uma milestone por vez; cada checkbox marcado num `M*.md` deve corresponder a um commit
ou PR rastreável — ver `docs/contributing/DEFINITION_OF_DONE.md`.

## Legenda de status usada em cada `M*.md`

- ✅ **Concluído** — verificado no código atual, não só na intenção do documento.
- 🔶 **Parcial** — a base já existe no código; falta formalizar, documentar ou fechar uma lacuna
  específica (listada no próprio item). Não é "implementar do zero".
- ⬜ **Não iniciado**
- 🔒 **Bloqueado** — depende de algo fora do controle do fork (ex.: conta na AUR, capacidade do
  CLI oficial).
