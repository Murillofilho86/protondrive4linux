# M9 — Production Release

## M9-001 — End-to-end test matrix
**Status: ⬜ Não iniciado.** Arch+KDE, Arch+GNOME, Arch+COSMIC, Ubuntu+GNOME, Fedora+GNOME,
Debian — depende de M3 estar pelo menos parcialmente pronto.

## M9-002 — Revisão de segurança final
**Status: ⬜ Não iniciado como checklist formal**, mas boa parte já foi coberta pela auditoria
original (`SECURITY_AUDIT.md`): dependency audit, credenciais, permissões, IPC.

### Checklist
- [x] Dependency audit (auditoria original + M2-004 em CI).
- [x] Supply chain (parcial — falta M2-001 release signing).
- [x] Updater (parcial — checksum ok, assinatura falta).
- [ ] Installer (depende de M7-002).
- [ ] Vault (N/A — deprioritizado, ver `M6-encrypted-vault.md`).
- [x] Permissions (`0700`/`0600` em `state_dir`/config).
- [x] Credentials (delegado ao keyring via CLI oficial).
- [x] Logs (rotação implementada).
- [x] IPC (`status.json`, não socket — permissões corrigidas).
- [ ] Filesystem — path traversal como checklist explícito de pentest, não só revisão de código.
- [ ] Command injection — idem, formalizar como teste, não só revisão manual.

## M9-003 — Revisão de perda de dados
**Status: ✅ Revisão feita, correções mescladas; falta uma decisão (ver abaixo).** Overlap direto
com M1 (disaster suite, offline recovery). Não é uma issue nova — é o checkpoint de "M1 realmente
fechou" antes do release candidate. Cenários: `kill -9`, power loss, network loss, disk full,
permission denied, remote unavailable, local unavailable, conflict, rename, large file, large tree.

O relatório completo — método, achados, matriz de cenários com o teste que mostra cada um, limites
que ficam e o que não deu para verificar — está em
[`docs/DATA_LOSS_REVIEW.md`](../DATA_LOSS_REVIEW.md) (em inglês, como o resto da documentação de
código).

### Método
Ler o engine perguntando "como isto pode perder o arquivo de alguém?", escrever um teste para cada
resposta e **guardar só os que falham no código de antes** (11 falharam), corrigir, e depois
sabotar a correção de propósito para ver o teste certo falhar (mutation check). Isso pegou dois
testes cegos: um teste de desastre que matava só o processo pai (o CLI órfão terminava de escrever
o arquivo) e um teste da guarda de pasta que nunca chegava ao código que deveria exercitar.

### Achados corrigidos (todos com teste que falhava na `master`)
- [x] Download que morre no meio deixava arquivo parcial com o nome final, ou trocava a cópia boa
      por um pedaço; um download "bem-sucedido" truncado era enviado por cima da cópia boa do
      remoto; uma edição feita durante a execução era sobrescrita (**#85**, `src/staged.rs`).
- [x] Cópia de conflito sobrescrevia um arquivo existente com o mesmo nome (**#85**).
- [x] Raiz local vazia (HD externo desmontado), pasta ilegível no `sync` em lote, arquivo/pasta
      trocado por symlink e par apontado para outra pasta apagavam o remoto; escrita através de
      symlink para fora da raiz (**#86**).
- [x] Dois arquivos de mesmo tamanho com mtimes diferentes eram tomados por "drift": a edição
      local sumia dos dois lados (**#87**).
- [x] Apagar uma pasta levava junto arquivos que nunca sincronizaram (locais ou remotos); rename
      falso-positivo mandava o conteúdo antigo para o nome novo (**#87**).

### Cenários
- [x] `kill -9` / SIGTERM (inclusive no meio de um download, matando o grupo de processos).
- [x] Power loss — por raciocínio: baseline em SQLite WAL com `synchronous = FULL` (fixado por
      teste) e `fsync` do arquivo baixado *antes* do baseline. Não dá para rodar de verdade aqui.
- [x] Network loss (offline, timeout do CLI no meio de um download).
- [x] Disk full — **simulado** (download que escreve parte e falha); disco cheio de verdade não foi
      criado.
- [x] Permission denied (pasta/arquivo ilegível, destino sem escrita).
- [x] Remote unavailable / local unavailable (raiz ausente, raiz vazia, listagem falha).
- [x] Conflict, rename, large tree.
- [~] Large file — o download é verificado e atômico, mas o limite é de tempo: uma transferência
      que não termina em `cli_timeout_secs` (1800 s por padrão) é morta e tentada de novo para
      sempre. Arquivo de vários GB em link lento precisa de `cli.cli_timeout_secs` maior.

### Disjuntor de deleção em massa (era a decisão pendente; implementado depois da revisão)
- [x] Com `propagate_deletes` ligado, uma execução apaga no máximo o maior entre
      `max_delete_files` (100) e `max_delete_percent` (20 %) dos arquivos que o par rastreia;
      se passar disso **nada** é apagado (as transferências continuam) e o par fica com um erro
      que diz como seguir: `sync --allow-mass-delete NOME` (foi de propósito) ou
      `sync --resync NOME` (trazer os arquivos de volta). Um walk completo conta os arquivos que
      ainda existem em disco antes da primeira deleção (HD desmontado / `rm -rf` não apaga
      nada), e todos os workers de um walk gastam uma única franquia.
- [x] Na janela: faixa na página Atividade ("1.640 exclusões seguradas") com os botões **Restore them**
      (`resync`: traz os arquivos de volta) e **Delete them** (pede confirmação; vai para a lixeira
      do Proton / a sua), cartão da pasta em âmbar e uma notificação do tray por pasta. A trava só
      cai quando uma execução sobre a pasta **inteira** não segura nada: uma passada sobre
      poucas pastas editadas não a apaga, para a faixa não ir e vir enquanto se trabalha.
- Limite que sobra: deleções fatiadas em várias execuções, cada uma abaixo da franquia.
- Limites documentados em `docs/DATA_LOSS_REVIEW.md` § Limits (timeout vs arquivo grande, timeout
  mata só o processo filho, duas execuções ao mesmo tempo não são impedidas, nomes de arquivo
  não-UTF-8, lista de ignorados silenciosa, `size+mtime` como heurística).

### Gate
"Nenhum `risk:data-loss` crítico aberto": **atendido** — nenhum caso de perda conhecido segue sem
correção ou guarda. O que fica são os limites acima e a ressalva de que nada foi exercitado contra
o `proton-drive` real (sem conta de teste), nem com queda de energia ou disco cheio de verdade.

## M9-004 — Release candidate
**Status: ⬜ Não iniciado.** Criar `v1.0.0-rc1`.

### Gate
Nenhum `risk:data-loss` ou `risk:security` crítico aberto.

## M9-005 — Stable release
**Status: ⬜ Não iniciado.** Criar `v1.0.0` — somente após:
- [ ] RC validado.
- [ ] Packages validados (M3, M7).
- [ ] Flatpak validado (se M3-005 decidiu seguir com ele).
- [ ] Documentation completa.
- [ ] Installation, upgrade, uninstall, recovery testados.
