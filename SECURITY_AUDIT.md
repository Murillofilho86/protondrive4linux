# Security Audit — Fase 0 (Auditoria de Due Diligence do Código Herdado)

Auditoria do código herdado do fork `WilhelmZA/protondrive_linux_sync` ("NeutronSync"),
neste momento em `Murillofilho86/protondrive_linux_sync`, branch `rust`, HEAD `f72d8ab`.
Segue a estrutura de `ROADMAP.md` §Fase 0. Cada achado foi verificado lendo o código-fonte
real, não apenas a documentação (`README.md` / `docs/SYNC_MODEL.md`).

Classificação: **Crítico** (corrigir antes de qualquer uso real) / **Alto** (corrigir antes
da Fase 1 fechar) / **Médio** (corrigir na Fase 1–2) / **Baixo** (aceitar risco ou corrigir
oportunisticamente). Cada item tem uma decisão explícita.

---

## 0.1 — Proveniência e integridade da cadeia de build

| Achado | Severidade | Decisão |
|---|---|---|
| Autor único (`WilhelmZA <82225237+WilhelmZA@users.noreply.github.com>`) em todos os 27 commits, identidade consistente (GitHub noreply, sem alternância de nome/e-mail), sem saltos temporais suspeitos — histórico de ~1 mês (2026-07-23 a 2026-08-26), cadência de commits plausível para trabalho solo. | Baixo | Aceitar risco. Nada indica manipulação de histórico, mas confirma o princípio orientador: é código de um único desenvolvedor, sem revisão externa. |
| **`git tag -l` não retorna nenhuma tag neste clone**, local ou remoto (`git ls-remote --tags origin` também vazio — o próprio fork ainda não tem tags). A alegação do README ("Commits and tags are GPG-signed") portanto **não pôde ser verificada** contra o upstream real a partir deste repositório; `scripts/release.sh` relata "se a tag foi assinada" mas isso não foi observado em produção aqui. | Médio | Corrigir depois: ao cortar a primeira release do fork com `scripts/release.sh`, confirmar manualmente que a tag sai assinada (`git verify-tag`) antes de promover. Não bloqueia a Fase 0, mas é uma verificação pendente da Fase 1. |
| `cargo deny check advisories` (RustSec DB, config padrão) rodou limpo: **nenhuma vulnerabilidade conhecida** nas 533 crates do `Cargo.lock`, incluindo as da feature `gui` (`eframe 0.35.0`, `gtk 0.18.2`, `tray-icon 0.24.1` confirmados presentes no lockfile). `cargo-audit` não está instalado localmente; é redundante com `cargo deny` (mesma base RustSec) então não foi instalado. | Baixo | Aceitar. Adicionar `cargo deny check` ao CI da Fase 1 (já listado no roadmap) com um `deny.toml` próprio do fork em vez do config default, para também cobrir licenças e duplicatas. |
| O binário `proton-drive` **nunca é baixado automaticamente** pelo fork — é resolvido via `which()`/PATH (`protoncli.rs: resolve_binary`) e o README exige que o usuário o instale por conta própria. Não há supply-chain risk de "download silencioso" aqui. | — | Confirmado seguro, nenhuma ação necessária. |

---

## 0.2 — Superfície de execução de processo externo

Todos os pontos de `Command::new`/`process::Command` foram mapeados:

- `protoncli.rs:189` — invocação do `proton-drive` CLI (o caminho principal).
- `updater.rs:103,331,389` — `dpkg-query`/`rpm` (detecção), `pkexec` (instalação), `setsid` (restart).
- `trash.rs:19` — `gio trash` (delete local recuperável).
- `bin/gui.rs` — re-exec do próprio binário (tray↔janela), `kill`, `xdg-open`.

| Achado | Severidade | Decisão |
|---|---|---|
| Argumentos ao `proton-drive` são passados via `Command::args`/`.arg()` (array de argv), **nunca** via `sh -c` ou concatenação de string — confirmado com grep (`sh -c`, `Stdio::` não usados para shell). Caminhos remotos/locais entram como elementos de argv separados, com `--` de fim-de-opções antes de paths (`protoncli.rs:250-254`) e quoting de glob para destinos locais. **Não há injeção de shell.** | — | Confirmado seguro. |
| `credentials_store` (modo "keychain"/"pass"/"unsafe_file") vai por `env("PROTON_DRIVE_CREDENTIALS_STORE", ...)`, nunca por argv — mas isso não é um segredo, é só o modo de armazenamento. **O fork nunca manipula senha, mailbox password ou 2FA diretamente**: `login()` (`protoncli.rs:322`) roda `proton-drive auth login` com `.status()` herdando stdio, ou seja, o usuário interage direto com o fluxo (browser) do CLI oficial. Nenhum segredo passa por `/proc/<pid>/cmdline` do lado do fork. | — | Confirmado seguro — delega corretamente a autenticação ao binário oficial, como o princípio orientador exige. |
| `Command::new(&self.binary)` — quando `cli.binary` é o default `"proton-drive"` (não um caminho absoluto), a resolução usa o `$PATH` herdado do processo (nenhum `env_clear()` é chamado). Isso é o comportamento padrão de qualquer wrapper de CLI, mas é o clássico vetor de **path hijacking**: um `$PATH` com `.` antes dos diretórios do sistema, ou um binário malicioso em um diretório de usuário que precede `/usr/bin` no `$PATH`, seria executado no lugar do `proton-drive` real. | Médio | Corrigir na Fase 1: o `PKGBUILD` deve fixar `cli.binary` (ou documentar) para o caminho absoluto `/usr/bin/proton-drive` no config de exemplo, eliminando a dependência do `$PATH` ambiente em produção. Baixo esforço, elimina a superfície. |
| Nomes de nó remotos (`extract_name`/`is_safe_component`, `protoncli.rs:364-371`) já são filtrados contra path traversal (separador de caminho, `.`/`..`, NUL) antes de virar caminho local — importante porque nomes remotos podem vir de pastas compartilhadas por terceiros. | — | Confirmado seguro, boa prática já presente. |

---

## 0.3 — Armazenamento de credenciais e estado

| Achado | Severidade | Decisão |
|---|---|---|
| O fork **não duplica sessão/token**: toda a gestão de credenciais fica delegada ao keyring do SO via o `proton-drive` CLI (confirmado — não há nenhuma leitura/escrita de token, senha ou cookie de sessão em nenhum módulo do fork). | — | Confirmado seguro. |
| Diretório de logs criado explicitamente com `mode(0o700)` e o arquivo `sync.log` com `mode(0o600)` (`logger.rs:36-43,209`), com o comentário correto no próprio código: "Logs record decrypted file paths, so keep them private to the user." | — | Confirmado seguro. |
| **Inconsistência**: `state_dir` (que guarda `stats.db` — baseline com caminhos, tamanhos, mtimes e hashes SHA-1 de todo o conteúdo sincronizado — e `status.json`, que inclui `current_op` com caminhos de arquivo) é criado só com `std::fs::create_dir_all` (`stats.rs:37`, `config.rs:635`), **sem** `mode(0o700)`. Com um `umask` padrão (022), o diretório fica `0755` e os arquivos dentro dele (`stats.db`, `status.json`, `baselines/*.json`) ficam `0644` — legíveis por qualquer outro usuário local. O próprio código já reconhece (em `logger.rs`) que esse tipo de dado é sensível o bastante para justificar `0700`/`0600`, mas não aplicou o mesmo padrão ao `state_dir`. Em uma máquina multiusuário, isso vaza a árvore de nomes de arquivo, tamanhos e hashes SHA-1 do Proton Drive do usuário para qualquer outra conta local. | **Alto** | Corrigir na Fase 1/2 antes de qualquer uso real: aplicar `DirBuilder::new().mode(0o700)` na criação de `state_dir` (em `stats.rs::open` e em `config::save`'s `create_dir_all(parent)`), e considerar retroativamente `chmod` num upgrade path se o diretório já existir com permissões abertas. |
| O arquivo de config TOML (`config::save`, `config.rs:632-639`) também usa `create_dir_all`/`std::fs::write` sem `mode()` explícito — fica no padrão do umask. Não contém segredos (paths e flags apenas), mas revela nomes de pares e caminhos locais/remotos. | Baixo | Aceitar por ora; opcionalmente alinhar com o padrão 0700/0600 dos logs na mesma correção acima, já que o custo é o mesmo `DirBuilderExt`/`OpenOptionsExt` já usado em `logger.rs`. |
| Nenhuma referência a `PROTON_DRIVE_UNSAFE_SECRETS` em nenhum lugar do fork — o fork nunca seta, lê nem ativa esse modo, silenciosamente ou não. | — | Confirmado seguro — não há ativação implícita de fallback inseguro. |

---

## 0.4 — Modelo de sync: corretude e segurança de dados

`docs/SYNC_MODEL.md` foi lido e cada invariante alegada foi checada contra `src/engine.rs` e
`tests/engine.rs` (38 testes). A maior parte das alegações **está** implementada e **está**
coberta por teste adversarial real (não apenas documentada):

- Raiz local ausente (mount desmontado) → recusa (`engine.rs:806-817`), testado em
  `missing_local_root_refuses_delete`.
- Baseline ausente → união em vez de espelhamento (`engine.rs:796-801`), exercitado
  implicitamente em `initial_upload`/`download_new_remote`.
- Falha de download/upload no meio da transferência → não gravado como sincronizado, testado
  em `failed_download_is_pending_not_deleted` e `failed_upload_keeps_local_edit`.
- Execução cancelada → linhas de baseline não tocadas ficam intactas, testado em
  `cancelled_run_does_not_record_untransferred_as_synced`.
- Listagem remota incompleta → sincroniza sem deletar, testado em
  `incomplete_remote_scan_suppresses_delete` e `shallow_remote_notfound_suppresses_delete`.
- Excludes nunca tocam o lado remoto, testado em `excluding_after_sync_touches_neither_side`,
  `reinclude_after_local_removal_redownloads_never_deletes_remote`.

### Achado central desta seção

| Achado | Severidade | Decisão |
|---|---|---|
| **A pasta-base remota do par inteiro sumir (`remote_root_missing`, `engine.rs:845-859`) — o cenário que `docs/SYNC_MODEL.md` descreve explicitamente como "catastrophe signature" e regra que "existe porque quebrá-la causou um incidente real" — tem cobertura de teste ZERO no suite atual**, apesar de haver 38 testes usando `FakeRemote`. Motivo: `FakeRemote` não sobrescreve `Remote::list_tree` (`protoncli.rs:67-116`); herda a implementação-padrão do trait, que (a) chama `self.list_dir()` em vez de `self.list_dir_probe()` — então nunca vê o `NotFound` que `FakeRemote::list_dir_probe` simula — e (b) **hard-coda `root_missing: false`** com o comentário explícito "only the concurrent walk detects it. Report 'present' here." Isso significa: todo teste que passa por `Engine::sync_pair` (a via usada por praticamente todos os 38 testes, via os helpers `run`/`try_run`) nunca exercita o branch `remote_root_missing` — só a implementação real e não testada `ProtonCli::list_tree` (`protoncli.rs:418-521`) o alcança. O teste que mais se aproxima (`shallow_remote_notfound_suppresses_delete`) cobre um cenário diferente e mais simples: uma SUBPASTA sumida durante um `sync_pair_shallow`, que passa por `list_dir_probe` diretamente, não pela raiz do par inteiro via `list_tree`. Um refactor futuro no walk concorrente de `ProtonCli::list_tree` poderia quebrar silenciosamente essa detecção sem que `cargo test` acusasse nada. | **Alto** | Corrigir na Fase 2 (antes de expandir escopo): dar a `FakeRemote` um `list_tree` próprio que honre `notfound_dir` também no nível da raiz do par e retorne `root_missing: true` nesse caso — depois escrever `vanished_remote_base_suppresses_delete` (par inteiro, não subpasta) espelhando `missing_local_root_refuses_delete`, mas para o lado remoto. Sem isso, a alegação central de segurança de dados do documento de design não está provada, só documentada. |
| `compare = "size+mtime"` (padrão do upstream) pode não detectar uma edição in-place que preserva tamanho e mtime — o próprio README já documenta essa limitação e oferece `compare = "sha1"` como alternativa mais cara. | Médio | Decisão de produto pendente para a Fase 2 (já listada no roadmap): considerar `sha1` como padrão do fork. Não é uma falha de segurança, é um trade-off de corretude vs. performance já transparente. |

---

## 0.5 — GUI e superfície de tray/IPC

| Achado | Severidade | Decisão |
|---|---|---|
| A comunicação daemon↔janela é **um arquivo simples** (`<state_dir>/status.json`, escrito atomicamente via write-temp+rename em `service.rs:435-450`), não um socket TCP/Unix nem D-Bus exposto — não há superfície de IPC "sem controle de acesso" no sentido de rede; o controle de acesso é o de arquivo do próprio SO. | — | Mas herda diretamente o achado de 0.3: como `state_dir` não é `0700`, `status.json` (que contém a atividade recente com caminhos de arquivo) também fica legível por outros usuários locais. Coberto pela mesma correção do item 0.3 (Alto). |
| Dependências de GUI fixadas no `Cargo.lock` (`eframe 0.35.0`, `gtk 0.18.2`, `tray-icon 0.24.1`, `egui-phosphor`, `rfd`) — checadas junto com o resto do lockfile por `cargo deny check advisories`: nenhuma advisory conhecida. | — | Confirmado seguro no momento desta auditoria; deve ser reexecutado a cada bump de dependência (Fase 1 CI). |

---

## 0.6 — Auto-update

Fluxo lido por completo em `updater.rs` e no chamador em `bin/gui.rs:2598-2644`.

| Achado | Severidade | Decisão |
|---|---|---|
| Ordem de operações confirmada correta e sequencial: `download()` → `verify()` → só então `install()` (`bin/gui.rs:2611-2644`); uma falha de `verify()` remove o arquivo baixado e retorna antes de chegar perto do `pkexec` (`gui.rs:2624-2629`). Diretório de download é `0700`, nomeado por PID (`updater.rs:246-257`), então uma janela de TOCTOU exigiria já ter execução de código como o mesmo usuário — nesse caso o auto-update deixa de ser o vetor relevante. | — | Confirmado seguro nesse desenho específico. |
| `verify()` falha fechado quando o release não publica digest (`updater.rs:288-295`, testado em `verify_fails_closed_when_the_release_published_no_digest`) — bom design defensivo. | — | Confirmado seguro. |
| **O "checksum" verificado é o campo `digest` que a própria API de Releases do GitHub calcula sobre o asset já publicado — não é uma assinatura do mantenedor verificada contra uma chave pública fixa.** O comentário do próprio código admite isso: "the digest arrives over TLS in the same API response as the version number, so it is not something the download itself can influence" — ou seja, protege contra corrupção de download/CDN, **mas não contra um pipeline de release ou conta do GitHub comprometidos**: se um atacante publica um asset malicioso, o GitHub recalcula o digest sobre esse asset malicioso e a verificação passa normalmente. Isso é mais fraco do que a alegação do README de "Signed releases" sugere para quem lê o fluxo de auto-update esperando uma assinatura GPG/minisign verificada contra uma chave do mantenedor fixada no binário. (A assinatura GPG das tags/commits, que existe, não é a mesma coisa: não cobre os binários `.deb`/`.rpm`/tarball publicados.) | **Alto** | Decisão de design do roadmap (0.6 já antecipa isso): **desabilitar auto-update na v0 do fork** e mover atualização só via AUR (`pacman -Syu`), como o próprio roadmap sugere — até que exista uma assinatura independente (ex.: minisign com chave pública embutida no binário) verificada antes do `pkexec`. Recomendo executar essa decisão já na Fase 1, não adiar. |
| **`updater.rs:23` tem `REPO: &str = "WilhelmZA/protondrive_linux_sync"` fixo (hard-coded) — o repositório upstream original, não o fork atual (`Murillofilho86/protondrive_linux_sync`, confirmado via `git remote -v`).** Isso não é uma vulnerabilidade abstrata: é um problema concreto e imediato para ESTE fork — se o auto-update GUI for habilitado como está, ele checa releases do repositório do WilhelmZA, não do fork, e (combinado com `install_kind()` checando o pacote `neutronsync` via `dpkg-query`/`rpm`) tentaria instalar builds do upstream sobre uma instalação que a Fase 1 pretende renomear. | **Crítico** (para este fork especificamente, não para o upstream) | Corrigir antes de habilitar `gui`/update em qualquer instalação real: atualizar `REPO` para o repositório do fork (ou melhor, mover para uma constante de config/build-time) como parte do rebranding da Fase 1 — e isso reforça a recomendação acima de manter auto-update desabilitado até o rebranding e a decisão de assinatura estarem resolvidos. |

---

## Resumo executivo

| # | Achado | Severidade | Status |
|---|---|---|---|
| 1 | `REPO` do updater aponta pro repo upstream, não pro fork | **Crítico** | **Corrigido** — `updater.rs`: `REPO` agora aponta para `Murillofilho86/protondrive_linux_sync` |
| 2 | `state_dir` (baseline + status.json) sem `0700`/`0600` | **Alto** | **Corrigido** — `stats.rs::open` cria `state_dir` com `mode(0o700)`; `config::save` cria o dir do config TOML com `0700` e o arquivo com `0600` |
| 3 | Auto-update: digest vem do mesmo canal que o asset, não é assinatura independente | **Alto** | **Corrigido (mitigado)** — `updater::UPDATES_ENABLED = false` desliga `check()` (usado tanto pelo botão manual quanto pelo `check_on_launch`); `download`/`verify`/`install` ficam inalcançáveis. Reabilitar só depois de uma assinatura independente (ex.: minisign) checada contra chave do mantenedor |
| 4 | Zero cobertura de teste para "pasta-base remota do par inteiro sumiu" (`remote_root_missing`) | **Alto** | **Corrigido** — `FakeRemote` ganhou um `list_tree` próprio (espelha a distinção raiz-vs-subpasta de `ProtonCli::list_tree`) e o teste `vanished_remote_base_suppresses_delete` foi adicionado a `tests/engine.rs` (39 testes, todos passando) |
| 5 | Tags do fork ainda não existem/verificadas com GPG | Médio | Pendente — verificar na primeira release |
| 6 | `cli.binary` resolvido via `$PATH` ambiente (path hijacking teórico) | Médio | Pendente — fixar caminho absoluto no PKGBUILD (Fase 1) |
| 7 | Config TOML sem `0600`/`0700` explícito | Baixo | **Corrigido junto do item 2** |
| 8 | `compare = "size+mtime"` pode não detectar edição in-place | Médio | Decisão de produto — Fase 2 |

Os quatro itens **Crítico**/**Alto** foram corrigidos nesta passada (`cargo test`: 61 testes,
todos verdes — 22 unitários + 39 em `tests/engine.rs`). Os itens 5, 6 e 8 ficam para a Fase 1/2
como já indicado; o item 5 em particular só pode ser verificado quando a primeira tag do fork
for cortada com `scripts/release.sh`.

## O que NÃO foi coberto nesta passada (gaps conhecidos desta auditoria)

- Testes de propriedade (`proptest`) para o classificador de mudanças — é trabalho de
  implementação, não de auditoria; fica para a Fase 2 como já planejado no roadmap.
- Corrida real entre `watch` (inotify) e o rescan periódico sob carga — não reproduzida aqui
  (exigiria um ambiente com o `proton-drive` CLI real); a mitigação por design (debounce +
  reconciliação aditiva por pasta) foi lida no código mas não teve teste de carga.
- CVEs de dependências do sistema fora do Cargo (glib/gtk3/libayatana no nível de pacote Arch)
  — fora do escopo de `cargo deny`; cabe ao empacotamento da Fase 5 usar as versões do
  repositório oficial do Arch.
