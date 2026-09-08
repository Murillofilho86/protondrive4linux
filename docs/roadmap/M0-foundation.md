# M0 — Foundation & Architecture

**Objetivo**: contratos arquiteturais que evitem acoplar GUI, sync, cripto e distribuição.

Ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md` para o mapeamento de módulos e as decisões
de contexto que este milestone formaliza.

## M0-001 — Arquitetura de módulos
**Labels**: `type:documentation`, `area:sync`, `priority:high`
**Status: 🔶 Parcial.** `CLAUDE.md` e `docs/architecture/ARCHITECTURE_PRINCIPLES.md` já
documentam os limites de módulo, e a GUI já depende só de `service::Controller`
(`docs/GUI_API.md`), nunca do CLI diretamente — o critério de aceite mais importante já está
satisfeito.

### Critérios de aceite
- [x] GUI não depende diretamente do Proton CLI.
- [x] Sync Engine não conhece detalhes da GUI.
- [x] Remote Provider possui interface explícita (`protoncli::ProtonCliTrait` — nome real a
      confirmar no código).
- [x] Storage/Baseline possui interface explícita (`state`).
- [x] Decisões arquiteturais documentadas (`docs/architecture/ARCHITECTURE_PRINCIPLES.md`).
- [ ] Diagrama arquitetural (imagem/mermaid) — falta.

## M0-002 — Contratos do Sync Engine
**Labels**: `type:refactor`, `area:sync`, `priority:high`
**Status: 🔶 Parcial.** Os invariantes centrais já existem e são testados: baseline só avança
após confirmação positiva da transferência, erro não corrompe estado global silenciosamente.
**Fundir com M1-002 ao abrir no GitHub** — mesma pauta, critério de aceite mais concreto lá.

### Critérios de aceite
- [ ] Sync Plan separado de Sync Execution (como conceito de código, hoje é implícito).
- [x] Operações de filesystem são representadas explicitamente (`Action` enum em `models.rs`).
- [x] Resultado de uma operação possui estado de sucesso/falha.
- [x] Baseline só pode ser atualizado após confirmação.
- [x] Erros não podem alterar silenciosamente o estado global.

## M0-003 — Matriz de estados de sincronização
**Labels**: `type:documentation`, `area:sync`, `area:testing`
**Status: ⬜ Não iniciado (como documento).** O comportamento para `LOCAL × BASELINE × REMOTE`
já existe implementado e coberto por `tests/engine.rs`, mas nunca foi extraído como tabela de
referência legível fora do código. Trabalho é 100% documentação — sem risco.

### Critérios de aceite
- [ ] Documentar todos os estados: `Created, Modified, Deleted, Unchanged, Conflict, Missing,
      Unavailable, Excluded, Pending, Failed`.
- [ ] Cada combinação `LOCAL × BASELINE × REMOTE` tem comportamento documentado, referenciando
      o teste em `tests/engine.rs` que a comprova.
