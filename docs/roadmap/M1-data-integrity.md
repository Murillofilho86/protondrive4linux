# M1 — Data Integrity

**Objetivo**: sincronização confiável diante de falhas, interrupções, corrupção ou edição
simultânea. **Este é o milestone mais importante do projeto** (princípio de priorização, item 1
— ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md`).

O projeto já parte de uma base sólida: baseline por arquivo, deleções conservadoras (opt-in,
recuperáveis), proteção contra root local ausente ou root remoto desaparecido, conflitos que
nunca destroem dados, baseline ausente une os dois lados em vez de espelhar.

## M1-001 — Disaster test suite
**Labels**: `type:test`, `area:testing`, `risk:data-loss`, `priority:critical`
**Status: 🔶 Parcial → gap real.** `tests/engine.rs` já cobre baseline ausente, root remoto
desaparecido (`vanished_remote_base_suppresses_delete`) e falha no meio da transferência via
`FakeRemote`.

### Cenários pendentes (`tests/disaster/`, novo)
- [ ] SIGTERM durante sincronização (processo real, não `FakeRemote`).
- [ ] SIGKILL durante sincronização.
- [ ] Disco cheio.
- [ ] Timeout de rede real.
- [ ] Proton CLI encerrado inesperadamente.
- [ ] Corrida real entre watch (inotify) e rescan periódico sob carga — exige o CLI real, não
      dá para simular só com `FakeRemote`.

### Critério de aceite
Nenhum cenário pode resultar em exclusão silenciosa ou perda de dados.

## M1-002 — Transactional sync execution
**Labels**: `type:refactor`, `area:sync`, `risk:data-loss`
**Status: 🔶 Parcial.** "Transferência concluída antes da baseline" e "estado intermediário
recuperável" já são verdade hoje (commit é por arquivo, não em lote).

```text
SCAN → PLAN → EXECUTE → VERIFY → COMMIT
```

### Critérios de aceite
- [x] Transferência concluída antes da baseline.
- [ ] Operação parcialmente executada pode ser retomada — formalizar como garantia testada, não
      consequência incidental do design atual.
- [x] Falha não invalida operações independentes.
- [x] Estado intermediário é recuperável.

## M1-003 — Baseline integrity
**Labels**: `type:feature`, `area:sync`, `risk:data-loss`
**Status: ⬜ Não iniciado — gap real.** Não há hoje detecção de baseline SQLite corrompida ou de
versão de schema incompatível. É o item de maior risco desta milestone: uma baseline corrompida
mal tratada pode ser lida como "todos os arquivos sumiram".

### Critérios de aceite
- [ ] Detectar baseline inválida.
- [ ] Detectar versão incompatível.
- [ ] Detectar corrupção.
- [ ] Nunca interpretar baseline corrompida como "todos os arquivos foram removidos".
- [ ] Recovery seguro.

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
**Status: 🔶 Parcial.** Já existe `ConflictPolicy::{KeepBoth, Newer, Skip}` configurável, com
seletor na GUI.

### Gap real
Não existe uma "caixa de entrada de conflitos" na GUI (lista dos arquivos em conflito aguardando
decisão) nem histórico de qual resolução foi aplicada a qual arquivo — hoje só existe a política
global. Fechar como feature de GUI + log, não como engine.

### Critérios de aceite
- [x] Conflito nunca destrói automaticamente a outra versão (`keep-both` é sempre seguro).
- [x] CLI/config suporta resolução (política global).
- [ ] GUI exibe conflitos (lista/inbox por arquivo).
- [ ] Histórico de resolução disponível no log.

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
**Status: ⬜ Não iniciado — gap real.** O cenário "internet cai, edita local, internet volta,
reconcilia" não tem teste dedicado hoje, mesmo que o mecanismo de baseline deva lidar com ele
implicitamente.

### Critérios de aceite
- [ ] Alterações locais preservadas.
- [ ] Alterações remotas recuperadas.
- [ ] Conflitos detectados.
- [ ] Nenhuma operação destrutiva incorreta.

## M1 — Definition of Done
- [ ] Disaster suite (parte nova) passando.
- [ ] Baseline integrity check implementado e testado.
- [ ] Conflict inbox na GUI.
- [ ] Offline recovery testado.
- [ ] Nenhum `risk:data-loss` crítico aberto.
