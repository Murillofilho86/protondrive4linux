# M0 — Foundation & Architecture

**Objetivo**: contratos arquiteturais que evitem acoplar GUI, sync, cripto e distribuição.

Ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md` para o mapeamento de módulos, o diagrama e as
decisões de contexto que este milestone formaliza. Ver `docs/architecture/SYNC_STATE_MATRIX.md`
para a matriz completa de M0-003.

## M0-001 — Arquitetura de módulos
**Labels**: `type:documentation`, `area:sync`, `priority:high`
**Status: ✅ Concluído.** `CLAUDE.md` e `docs/architecture/ARCHITECTURE_PRINCIPLES.md` já
documentam os limites de módulo, a GUI já depende só de `service::Controller`
(`docs/GUI_API.md`), nunca do CLI diretamente, e o diagrama arquitetural (mermaid) foi adicionado
a `ARCHITECTURE_PRINCIPLES.md` § Diagrama arquitetural.

### Critérios de aceite
- [x] GUI não depende diretamente do Proton CLI.
- [x] Sync Engine não conhece detalhes da GUI.
- [x] Remote Provider possui interface explícita (`protoncli::Remote`, trait — confirmado no
      código).
- [x] Storage/Baseline possui interface explícita (`state`).
- [x] Decisões arquiteturais documentadas (`docs/architecture/ARCHITECTURE_PRINCIPLES.md`).
- [x] Diagrama arquitetural (mermaid, em `ARCHITECTURE_PRINCIPLES.md`).

## M0-002 — Contratos do Sync Engine
**Labels**: `type:refactor`, `area:sync`, `priority:high`
**Status: 🔶 Quase concluído.** Investigação em `src/engine.rs` confirmou que a separação
`SCAN → PLAN → EXECUTE → VERIFY → COMMIT` **já existe no código** — não é um refactor pendente,
era falta de reconhecer/documentar o que já está lá. Detalhado em
`ARCHITECTURE_PRINCIPLES.md` § Sync Plan vs. Sync Execution: `Plan { ops: Vec<Op> }`
(`models.rs`) já separa o cálculo (função pura `decide`/`decide_dir`) da execução (loop que
consome `plan.ops`), `apply_op() -> Result<()>` já é o VERIFY, e o bookkeeping `pending`/`done`
já é o COMMIT por confirmação positiva.

**Único item real pendente, compartilhado com M1-002** (`docs/roadmap/M1-data-integrity.md`):
formalizar "operação parcialmente executada pode ser retomada" como garantia *testada*, não só
consequência do design. Não abrir uma issue nova para isso — já está rastreado em M1-002.

### Critérios de aceite
- [x] Sync Plan separado de Sync Execution — confirmado, já existe (`Plan`/`Op` vs. o loop de
      execução).
- [x] Operações de filesystem são representadas explicitamente (`Action` enum em `models.rs`).
- [x] Resultado de uma operação possui estado de sucesso/falha (`apply_op() -> Result<()>`).
- [x] Baseline só pode ser atualizado após confirmação (`pending`/`done`).
- [x] Erros não podem alterar silenciosamente o estado global.

## M0-003 — Matriz de estados de sincronização
**Labels**: `type:documentation`, `area:sync`, `area:testing`
**Status: ✅ Concluído.** `docs/architecture/SYNC_STATE_MATRIX.md` documenta a tabela completa
`LOCAL × REMOTE` (por baseline presente/ausente), a tabela de diretórios, a política de
conflito, e as renomeações — extraída lendo `classify`/`decide`/`decide_dir` em `src/engine.rs`,
citando o teste de `tests/engine.rs` que comprova cada linha onde existe um.

### Critérios de aceite
- [x] Documentar todos os estados relevantes (`Created, Modified, Deleted, Unchanged, Absent`,
      mais `Conflict` como resultado, não como estado de entrada — ver nota abaixo).
- [x] Cada combinação alcançável `LOCAL × BASELINE × REMOTE` tem comportamento documentado,
      referenciando o teste em `tests/engine.rs` que a comprova (onde existe um).

**Nota**: `Missing`, `Unavailable`, `Excluded`, `Pending`, `Failed` do enunciado original da
issue não são estados do classificador (`Change` em `models.rs` só tem `Unchanged, Created,
Modified, Deleted, Absent`) — são conceitos de camadas diferentes: `Excluded` é filtrado antes de
chegar no classificador (`pair.is_excluded`), `Pending`/`Failed` são estados do bookkeeping de
execução (`pending`/`done` sets), não do classificador em si, e `Missing`/`Unavailable` mapeiam
para os guards de scan incompleto (`incomplete`, `root_missing`) documentados em
`docs/SYNC_MODEL.md`, não para uma entrada da matriz por-arquivo. `SYNC_STATE_MATRIX.md` cobre
os dois com uma nota, em vez de forçar uma coluna que não existe no código.

### Gap encontrado durante a investigação (não fechado por este item, alimenta M1)
`SYNC_STATE_MATRIX.md` § Coverage gaps lista comportamentos implementados mas sem teste
dedicado — o mais importante: `ConflictPolicy::Newer` e `ConflictPolicy::Skip` não têm nenhum
teste (só `KeepBoth` é exercitado). Isso é `risk:data-loss` e foi adicionado como nota em
`M1-data-integrity.md` M1-005, não uma issue nova.
