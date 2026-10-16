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
**Status: ✅ Fechado.** Um arquivo cujo upload/download falha não é mais reatacado já na
próxima rodada — fica em backoff exponencial (`retry_base_delay_secs`, dobra a cada falha
consecutiva, teto em `retry_max_delay_secs`) com jitter determinístico derivado do próprio
caminho do arquivo (sem depender de uma crate `rand`), então muitos arquivos falhando juntos
(ex.: sob rate-limit da API da Proton) não tentam de novo em lockstep. O crescimento do delay
estaciona após `retry_max_attempts` falhas consecutivas — continua tentando no delay máximo
indefinidamente depois disso, nunca desiste de vez (um problema transitório, como rate-limit ou
uma instabilidade de rede, tende a passar). Estado persistido em `stats.db` (tabela
`retry_state`, por `pair`+`rel`), então sobrevive a um restart do daemon/GUI. Testado com
`FakeRemote` em `tests/engine.rs`: confirma que um item em backoff é pulado (zero tentativas,
não "tentou e falhou de novo") e que o backoff é limpo assim que a transferência tem sucesso.

### Critérios de aceite
- [x] Exponential backoff (`retry_base_delay_secs`, dobra por tentativa).
- [x] Jitter (determinístico, por caminho — sem nova dependência de crate).
- [x] Max attempts configurável (`retry_max_attempts`; estaciona o delay, não desiste).

## M4-005 — Bandwidth limits
**Labels**: `type:feature`, `area:performance`
**Status: ✅ Fechado.** `upload_limit_kbps`/`download_limit_kbps` (KB/s, 0 = ilimitado,
compartilhado entre todos os pairs e workers) expostos na GUI (Settings → BANDWIDTH) e no TOML.

O `proton-drive` CLI não expõe progresso byte a byte (é um subprocesso one-shot), então não há
como estrangular uma transferência no meio — em vez disso, cada transferência inteira é agendada
num "horário de partida" virtual calculado a partir do seu tamanho (`src/ratelimit.rs`,
`RateLimiter`): a transferência espera a sua vez e então roda em velocidade total. Um arquivo
grande com um limite baixo atrasa visivelmente o que está na fila atrás dele, em vez de ele
mesmo ser desacelerado — uma simplificação aceita dado que o CLI não oferece outra alavanca.

Compartilhado corretamente entre chamadores concorrentes: cada chamador recebe seu próprio slot
não sobreposto numa linha do tempo virtual, então a vazão combinada de todos juntos (múltiplos
workers de scan, pool de downloads) fica no limite configurado — não `threads x limite`. Sem
nova dependência de crate (`rand` não foi necessário, nem para M4-004 nem aqui).

### Critérios de aceite
- [x] GUI: `Upload limit`, `Download limit` (Settings → BANDWIDTH, 0 = ilimitado).
- [x] Compartilhado entre pairs/workers concorrentes (testado: `ratelimit::tests`,
      `tests/engine.rs::upload_limit_paces_the_transfer`/`download_limit_paces_the_transfer`).
