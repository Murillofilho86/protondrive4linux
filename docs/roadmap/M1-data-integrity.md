# M1 — Data Integrity

**Objetivo**: sincronização confiável diante de falhas, interrupções, corrupção ou edição
simultânea. **Este é o milestone mais importante do projeto** (princípio de priorização, item 1
— ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md`).

O projeto já parte de uma base sólida: baseline por arquivo, deleções conservadoras (opt-in,
recuperáveis), proteção contra root local ausente ou root remoto desaparecido, conflitos que
nunca destroem dados, baseline ausente une os dois lados em vez de espelhar.

## M1-001 — Disaster test suite
**Labels**: `type:test`, `area:testing`, `risk:data-loss`, `priority:critical`
**Status: 🔶 Quase concluído.** `tests/engine.rs` já cobria baseline ausente, root remoto
desaparecido (`vanished_remote_base_suppresses_delete`) e falha no meio da transferência via
`FakeRemote` — tudo em processo, sem sinal real. `tests/disaster/` (novo) cobre o que só um
subprocesso de verdade consegue exercitar: `tests/support/fake_proton_drive.rs` é um
stand-in mínimo do CLI oficial (list/upload/download/create-folder/trash/version contra uma
árvore de diretórios comum, sem rede), e o binário `protondrive4linux` real roda contra ele.

### Cenários cobertos (`tests/disaster/main.rs`)
- [x] SIGTERM durante sincronização (`sigterm_mid_sync_recovers_cleanly`) — mata o processo de
      verdade no meio de um lote de 100 uploads, confirma que nada local é perdido, e que uma
      segunda run converge 100% sem criar conflito espúrio.
- [x] SIGKILL durante sincronização (`sigkill_mid_sync_recovers_cleanly`) — mesma prova, sinal
      que não pode ser interceptado. **Achado**: o binário não tem handler de sinal algum (nem
      `ctrlc`, nem tratamento de `SIGTERM`/`SIGINT` em `main.rs` — `watch` roda "until Ctrl-C"
      via o comportamento padrão do SO). Isso significa que SIGTERM e SIGKILL já são
      equivalentes na prática: a segurança vem inteiramente do commit-por-confirmação-positiva
      (baseline só avança no fim do lote inteiro; um processo morto no meio não commita nada),
      não de um shutdown gracioso que não existe.
- [x] Proton CLI encerrado inesperadamente (`crashing_cli_does_not_corrupt_state`) — toda
      chamada ao CLI falha (`FAKE_CLI_CRASH`); a run reporta erros sem perder nada local nem
      tocar o remoto, e uma run seguinte com o CLI saudável converge normalmente.

### Cenários que ficam de fora, deliberadamente (não simulados por desonestidade, não por preguiça)
- **Disco cheio**: este ambiente (sandbox de dev/CI) não tem como criar uma condição real de
  disco cheio sem um mount com quota/privilégio — não fabricado.
- **Timeout de rede real**: o código não tem nenhum mecanismo de timeout hoje (`Command::output()`
  bloqueia indefinidamente) — não há o que testar até essa feature existir; ver `M2-003`/`M4`.
- **Corrida real entre watch e rescan periódico sob carga**: precisa do comportamento de
  cache/timing do CLI oficial de verdade — o objetivo desse cenário é justamente pegar
  peculiaridades do binário real, que um stub não reproduz com fidelidade. Continua exigindo
  acesso a uma conta Proton real para reproduzir.

> **Atualização (M9-003):** a revisão de perda de dados voltou a este milestone e encontrou
> defeitos reais que os testes daqui não pegavam (downloads não atômicos, guardas de deleção
> ausentes, a heurística "mesmo tamanho = mesmo conteúdo"). Estão corrigidos e cobertos em
> `tests/data_loss.rs` e `tests/disaster/`; **disco cheio** passou a ser *simulado* (download que
> falha no meio) e o **timeout de rede** passou a existir e ser testado (`cli_timeout_secs`). A
> corrida watch/rescan continua não coberta. Ver [`docs/DATA_LOSS_REVIEW.md`](../DATA_LOSS_REVIEW.md).

### Critério de aceite
Nenhum cenário coberto resultou em exclusão silenciosa ou perda de dados. Os três cenários acima
ficam documentados como não cobertos, não como "concluídos por omissão".

## M1-002 — Transactional sync execution
**Labels**: `type:refactor`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído** (mesma investigação de M0-002, ver
`docs/architecture/ARCHITECTURE_PRINCIPLES.md` § Sync Plan vs. Sync Execution). O pipeline já
existe no código, confirmado lendo `src/engine.rs`:

```text
SCAN → PLAN → EXECUTE → VERIFY → COMMIT
```

`Plan{ops}` (cálculo puro) → loop de execução → `apply_op() -> Result<()>` (verify) →
bookkeeping `pending`/`done` (commit por confirmação positiva). Não era um refactor pendente.

### Critérios de aceite
- [x] Transferência concluída antes da baseline.
- [x] Operação parcialmente executada pode ser retomada — já testado como contrato explícito,
      não só consequência do design: `failed_download_is_pending_not_deleted`,
      `failed_upload_keeps_local_edit` (falha e retomada, ambas direções) e
      `cancelled_run_does_not_record_untransferred_as_synced` (cancelamento a meio da run,
      depois retomada) em `tests/engine.rs`.
- [x] Falha não invalida operações independentes.
- [x] Estado intermediário é recuperável.

## M1-003 — Baseline integrity
**Labels**: `type:feature`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído.** Investigação encontrou que a garantia central já era estrutural, não
apenas incidental: `classify()` só pode emitir `Deleted` para um path presente na baseline —
uma baseline vazia (por corrupção degradada a "arquivo SQLite novo" ou por erro propagado) é
**estruturalmente incapaz** de gerar qualquer delete, nunca foi só "sorte de design". O que
realmente faltava era (a) um guard de versão de schema, que não existia, e (b) provar isso com
teste, não só por leitura de código.

### O que foi adicionado
- `PRAGMA user_version` em `Stats::open` (`src/stats.rs`): grava a versão do schema; recusa
  abrir (erro, não leitura silenciosa) se encontrar uma versão diferente da que este build
  entende. Versão `0` (arquivo novo ou DB de antes desta checagem existir) é aceita e
  estampada — não quebra instalações existentes.
- `open_refuses_a_corrupted_file_instead_of_silently_treating_it_as_empty` e
  `open_refuses_an_incompatible_schema_version` (`src/stats.rs`, testes unitários): provam que
  um arquivo corrompido (bytes que não são SQLite) e uma versão de schema estranha falham alto
  (`Err`), citando a mensagem real do SQLite, não uma checagem que poderia ela mesma ser
  contornada.
- `corrupted_baseline_db_refuses_to_sync_rather_than_mass_delete` (`tests/engine.rs`): prova de
  ponta a ponta — corrompe o `stats.db` de um par já sincronizado e confirma que a sync inteira
  é recusada (`Err`) e **nenhum arquivo é deletado de nenhum dos lados**.

### Recovery (a resposta a "Recovery seguro")
Falhar alto já É a recuperação segura: o usuário resolve apagando/restaurando o `stats.db`
corrompido e rodando de novo — nesse ponto cai no caminho já existente e testado de "baseline
ausente une os dois lados" (nunca espelha), então o pior caso é re-upload/re-download do que já
existia, nunca perda de dado. Não foi construído fluxo de "auto-reparo" — não é necessário dado
que a via seura já existe e é a mesma usada para qualquer baseline ausente.

### Critérios de aceite
- [x] Detectar baseline inválida (arquivo corrompido falha em `Stats::open`, propagado como
      `Err`, nunca lido como vazio).
- [x] Detectar versão incompatível (`PRAGMA user_version` guard, novo).
- [x] Detectar corrupção (erro nativo do SQLite já propagava; agora comprovado por teste).
- [x] Nunca interpretar baseline corrompida como "todos os arquivos foram removidos" — comprovado
      estruturalmente (`classify()`) e por teste de ponta a ponta.
- [x] Recovery seguro (mesma via já testada de "baseline ausente une, nunca espelha").

## M1-004 — Rename detection
**Labels**: `type:feature`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído**, com uma limitação documentada. Já existe `Action::RenameLocal` /
`RenameRemote` (`src/models.rs`), detectado por conteúdo.

### Limitação do CLI, não do fork
`proton-drive` não tem rename/move atômico (`src/protoncli.rs`: `"rename/move not supported by
this backend"`), então no lado remoto o "rename" ainda vira upload+delete. Não há o que corrigir
sem o CLI oficial expor a operação — fechar a issue documentando a limitação.

### Critérios de aceite
- [x] Detectar rename (local e remoto, por conteúdo).
- [x] Não gerar upload desnecessário quando é rename local.
- [x] Não gerar delete desnecessário quando é rename local.
- [x] Testar rename local, rename remoto, rename simultâneo (`tests/engine.rs`).

## M1-005 — Conflict Manager
**Labels**: `type:feature`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído.** `ConflictPolicy::{KeepBoth, Newer, Skip}` configurável, com seletor na
GUI e cobertura de teste completa das três políticas, e agora uma **caixa de entrada de
conflitos** na GUI (página *Conflicts*, com contador na barra lateral) com o histórico de como
cada um foi resolvido.

### Testes adicionados (fechando o gap encontrado em `SYNC_STATE_MATRIX.md`)
`conflict_newer_local_wins`, `conflict_newer_remote_wins`,
`conflict_newer_without_remote_mtime_falls_back_to_keep_both` (prova que `Newer` nunca *adivinha*
um vencedor sem mtime comparável dos dois lados — cai em `KeepBoth`, nunca escolhe um lado às
cegas) e `conflict_skip_leaves_both_sides_untouched`, todos em `tests/engine.rs`.

### A caixa de entrada (antes era o gap real restante)
Achado ao fazer isto: o engine não dizia a ninguém que um conflito aconteceu. `Skip` vira um
`Noop`, que nem emite evento de operação (não aparecia em lugar nenhum além de uma linha de log),
e `Newer` vira um upload/download comum, indistinguível de uma sincronização normal. Agora toda
política relata como resolveu, com um evento estruturado (`SyncEvent::Conflict { pair, path,
resolution, copy }`) emitido **só quando a resolução de fato terminou** — uma transferência que
falhou é re-detectada na rodada seguinte e não relata nada ainda; um dry run não relata nada.

- `KeptBoth` (com o nome da cópia preservada), `KeptLocal`/`KeptRemote` (`Newer`: quem ganhou) e
  `Skipped` (re-relatado a cada rodada até o usuário resolver; a caixa de entrada deduplica).
- Persistido na tabela `conflicts` do `stats.db` (compartilhado entre o daemon da bandeja, a
  janela e as execuções de CLI). Um conflito `Skipped` se resolve sozinho — sai da caixa — quando
  o arquivo finalmente sincroniza (o usuário igualou os dois lados, ou trocou a política).
- GUI: página **Conflicts** com contador na barra lateral; cada cartão diz o que aconteceu com as
  duas versões em linguagem simples, com **Show folder** e **Dismiss**; **Show resolved** mostra
  o histórico. `protondrive4linux-gui --page conflicts` abre direto nela (serve de deep link).
- CLI: `status` mostra quantos conflitos estão sem resolver; o `watch` headless (unit do systemd)
  agora grava no mesmo DB, então também alimenta a caixa de entrada e o feed de Activity (antes
  não gravava nada).
- Log de texto: cada resolução vira uma linha (`conflict on a.txt: kept both versions (copy: ...)`).

Limitação conhecida de `Newer`: o lado mais antigo é *substituído* (no remoto vai pra lixeira do
Proton; localmente o download sobrescreve) — a caixa de entrada avisa qual versão foi mantida,
mas não guarda a perdedora. Quem não quer isso usa `keep-both` (o padrão).

### Critérios de aceite
- [x] Conflito nunca destrói automaticamente a outra versão (`keep-both` é sempre seguro).
- [x] CLI/config suporta resolução (política global).
- [x] Teste para `ConflictPolicy::Newer` (incluindo o fallback para `KeepBoth` sem mtime).
- [x] Teste para `ConflictPolicy::Skip`.
- [x] GUI exibe conflitos (lista/inbox por arquivo).
- [x] Histórico de resolução disponível no log (linha no log de texto + tabela `conflicts` com
      "Show resolved" na GUI).

## M1-006 — Content verification
**Labels**: `type:feature`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído**, precisa só de rename/doc. Já existe `options.compare = size | size+mtime
| sha1` (`src/config.rs`), com degradação automática sha1→size+mtime→size quando falta hash
(`src/models.rs`).

### Critérios de aceite
- [x] Política configurável (`fast`=`size`, `balanced`=`size+mtime`, `paranoid`=`sha1` — mapear
      nomes na documentação de usuário).
- [x] Testes de arquivo com mesmo tamanho (`tests/engine.rs`).
- [x] Testes de mtime enganoso.
- [ ] Benchmark de cada modo — ver M4-001.
- [ ] Decidir se `sha1` deve virar o padrão do fork (decisão de produto, adiada desde a auditoria
      original — não é bug de segurança).

## M1-007 — Offline recovery
**Labels**: `type:test`, `area:sync`, `risk:data-loss`
**Status: ✅ Concluído.** `offline_then_reconnect_reconciles_correctly` (`tests/engine.rs`)
cobre o ciclo completo: `FakeRemote` ganhou um modo `offline` (toda chamada de list/upload/
download falha, como um `proton-drive` real sem rede) — edições locais feitas offline, uma
tentativa de sync que falha graciosamente (erros reportados, nada perdido), mudanças
independentes injetadas no remoto durante a "queda", e uma reconciliação final que confirma as
4 garantias abaixo numa única passada.

O mecanismo por trás já existia (o guard de "scan incompleto suprime deletes", o mesmo que
`incomplete_remote_scan_suppresses_delete`/`vanished_remote_base_suppresses_delete` já
testavam) — o que faltava era provar que ele cobre o fluxo offline→reconecta de ponta a ponta,
não só uma falha pontual de listagem.

### Critérios de aceite
- [x] Alterações locais preservadas (edição local + arquivo novo local sobrevivem à tentativa
      offline e são enviados ao reconectar).
- [x] Alterações remotas recuperadas (arquivo criado no remoto durante a queda é baixado ao
      reconectar).
- [x] Conflitos detectados (o mesmo arquivo editado nos dois lados durante a queda vira
      conflito de verdade, resolvido pela `ConflictPolicy`, nunca perdido).
- [x] Nenhuma operação destrutiva incorreta (arquivo intocado sobrevive; nada no remoto muda
      enquanto offline).

## M1 — Definition of Done
- [x] Disaster suite (parte nova) passando — 3/3 cenários testáveis neste ambiente; disco
      cheio, timeout de rede real, e a corrida watch/rescan seguem documentados como não
      cobertos (exigem CLI real ou infraestrutura privilegiada, não código faltando).
- [x] Baseline integrity check implementado e testado.
- [x] Conflict inbox na GUI.
- [x] Offline recovery testado.
- [x] Nenhum `risk:data-loss` crítico aberto (os 3 cenários fora de escopo são limitações de
      ambiente/CLI oficial documentadas, não riscos abertos do fork).
