# M4 — Performance

## M4-001 — Benchmark framework
**Labels**: `type:test`, `area:performance`
**Status: ⬜ Não iniciado.** Nunca foi medido com datasets de escala.

### Escopo
- Datasets: 1k / 10k / 100k / 1M arquivos.
- Tamanhos: 1 KB / 1 MB / 100 MB / 1 GB / 10 GB.
- Métricas: startup, scan, CPU, RAM, network, latency, throughput.

Sem isso, qualquer alegação de performance (inclusive as decisões de M1-006 sobre `sha1` como
padrão) é opinião, não medição.

## M4-002/M4-003 — Transfer queue e transferências paralelas
**Labels**: `type:feature`, `area:performance`
**Status: ✅ Concluído em essência.** Já existe worker pool dimensionado por
`available_parallelism` (capado, configurável via `scan_threads`), retry por item independente
(uma falha não cancela as outras), CLI cache isolado por worker (`src/engine.rs`).

### Gap real
Falta só nomear/expor os estados (`pending`/`running`/`retry`/`completed`/`failed`/`cancelled`)
como um conceito visível — hoje eles existem como comportamento, não como um objeto "Transfer
Queue" inspecionável (útil pra GUI mostrar progresso granular).

### Critérios de aceite
- [x] Limite configurável (`scan_threads`).
- [x] Não exceder rate limit — falta backoff explícito, ver M4-004.
- [x] Retry individual.
- [x] Falha de uma transferência não cancela todas.

## M4-004 — Retry/backoff exponencial com jitter
**Labels**: `type:feature`, `area:performance`
**Status: ⬜ Não iniciado — gap real.** Hoje um item que falha só "retry na próxima rodada"
(ciclo do watcher/rescan), sem backoff exponencial nem jitter. Sob rate-limit da API da Proton
isso pode gerar tentativas em lockstep.

### Critérios de aceite
- [ ] Exponential backoff.
- [ ] Jitter.
- [ ] Max attempts configurável.

## M4-005 — Bandwidth limits
**Labels**: `type:feature`, `area:performance`
**Status: ⬜ Não iniciado.**

### Escopo
GUI: `Upload limit`, `Download limit`.
