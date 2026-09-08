# GitHub Issue Inventory

Lista achatada de todas as issues descritas em `docs/roadmap/M*.md`, para uso ao rodar
`gh issue create` em lote. Status vem da leitura do código real (ver cada `M*.md` para o porquê).
Legenda de status em `GITHUB_PROJECT_SETUP.md`.

| ID | Título | Milestone | Labels | Status |
| --- | --- | --- | --- | --- |
| M0-001 | Definir arquitetura de módulos | M0 | `type:documentation` `area:sync` `priority:high` | 🔶 |
| M0-002 | Definir contratos do Sync Engine | M0 | `type:refactor` `area:sync` `priority:high` | 🔶 (fundir com M1-002) |
| M0-003 | Criar matriz de estados de sincronização | M0 | `type:documentation` `area:sync` `area:testing` | ⬜ |
| M1-001 | Disaster test suite | M1 | `type:test` `area:testing` `risk:data-loss` `priority:critical` | 🔶 |
| M1-002 | Transactional sync execution | M1 | `type:refactor` `area:sync` `risk:data-loss` | 🔶 |
| M1-003 | Baseline integrity | M1 | `type:feature` `area:sync` `risk:data-loss` | ⬜ |
| M1-004 | Rename detection | M1 | `type:feature` `area:sync` `risk:data-loss` | ✅ (limitação do CLI documentada) |
| M1-005 | Conflict Manager | M1 | `type:feature` `area:sync` `risk:data-loss` | 🔶 |
| M1-006 | Content verification | M1 | `type:feature` `area:sync` `risk:data-loss` | ✅ (falta benchmark) |
| M1-007 | Offline recovery | M1 | `type:test` `area:sync` `risk:data-loss` | ⬜ |
| M2-001 | Release signing | M2 | `type:security` `area:security` `priority:critical` | ⬜ |
| M2-002 | Secure updater | M2 | `type:security` `area:security` `risk:security` | 🔶 (desligado) |
| M2-003 | Proton CLI resolution hardening | M2 | `type:security` `area:security` `risk:security` | ⬜ |
| M2-004 | Automated dependency audit | M2 | `type:security` `area:security` | 🔶 |
| M2-005 | Reproducible release metadata | M2 | `type:security` `area:distribution` | 🔶 |
| M3-001 | Linux compatibility matrix | M3 | `type:documentation` `area:packaging` | ⬜ |
| M3-002 | Arch package | M3 | `area:packaging` `area:distribution` | 🔶 |
| M3-003 | Pacote Debian | M3 | `area:packaging` `area:distribution` | ⬜ |
| M3-004 | Pacote RPM | M3 | `area:packaging` `area:distribution` | ⬜ |
| M3-005 | Pesquisa de viabilidade Flatpak | M3 | `type:research` `area:distribution` `priority:high` | ⬜ |
| M4-001 | Benchmark framework | M4 | `type:test` `area:performance` | ⬜ |
| M4-002 | Transfer queue | M4 | `type:feature` `area:performance` | ✅ (essência) |
| M4-003 | Parallel transfers | M4 | `type:feature` `area:performance` | ✅ (essência) |
| M4-004 | Retry/backoff exponencial | M4 | `type:feature` `area:performance` | ⬜ |
| M4-005 | Bandwidth limits | M4 | `type:feature` `area:performance` | ⬜ |
| M5-001 | Sistema de temas | M5 | `type:feature` `area:gui` | ⬜ |
| M5-002 | Temas customizados | M5 | `type:feature` `area:gui` | ⬜ (bloqueado por M5-001) |
| M5-003 | File status indicators | M5 | `type:feature` `area:gui` | ⬜ |
| M5-004 | File manager integration | M5 | `type:feature` `area:gui` | ⬜ |
| M5-005 | Onboarding, tray, notificações | M5 | `type:feature` `area:gui` | ⬜ |
| M7-001 | Flatpak | M7 | `type:feature` `area:distribution` | 🔒 (bloqueado por M3-005) |
| M7-002 | Instalador universal (.sh) | M7 | `type:feature` `area:distribution` | ⬜ |
| M7-003 | `doctor` expandido | M7 | `type:feature` `area:packaging` | 🔶 |
| M7-004 | Publicação real na AUR | M7 | `area:distribution` | 🔒 (bloqueado externamente) |
| M7-005 | `.SRCINFO` + `namcap` | M7 | `area:packaging` | ⬜ |
| M9-001 | End-to-end test matrix | M9 | `type:test` `area:testing` | ⬜ |
| M9-002 | Revisão de segurança final | M9 | `type:security` `area:security` | 🔶 |
| M9-003 | Revisão de perda de dados | M9 | `type:test` `risk:data-loss` | 🔶 |
| M9-004 | Release candidate | M9 | `type:documentation` | ⬜ |
| M9-005 | Stable release | M9 | `type:documentation` | ⬜ |

`M6-*` (Encrypted Vault) e `M8-*` (Cloud Drive Features) **não entram nesta tabela** — não são
issues abertas no GitHub por decisão deliberada. Ver `M6-encrypted-vault.md` e
`M8-cloud-features.md`.

## Como criar as issues (referência)

```sh
gh issue create \
  --title "M1-003: Baseline integrity" \
  --body "Ver docs/roadmap/M1-data-integrity.md#m1-003--baseline-integrity" \
  --label "type:feature,area:sync,risk:data-loss" \
  --milestone "M1"
```

Repetir por linha da tabela acima, usando o `Milestone` da coluna correspondente. Itens já
✅/🔶 podem virar issues fechadas de cara (ou nem virar issue, dependendo de quanto o time quer
rastreabilidade de trabalho já feito) — critério é do dono do projeto, não automático.
