# M2 — Supply Chain Security

**Objetivo**: garantir que o binário distribuído é exatamente o artefato que o projeto produziu.

## M2-001 — Release signing
**Labels**: `type:security`, `area:security`, `priority:critical`
**Status: ✅ Fechado.** Modelo: GPG com chave mestra (offline, capacidade `Certify` só) +
subchave de assinatura (a que roda em CI) — ver `docs/contributing/RELEASE_SIGNING.md` pro
racional e o passo a passo. O workflow (`.github/workflows/release.yml`) importa a subchave,
assina cada artefato (`.deb`, `.rpm`, tarballs) e **falha a build** se a assinatura não sair.
Verificado de ponta a ponta na release `v0.4.2`: `gpg --verify` confirma `Good signature` nos
4 artefatos publicados.

Duas voltas até chegar lá, ambas corrigidas e documentadas para não se repetir:
- A chave original (gerada em 2026-09-09) teve a senha perdida no processo de configuração e foi
  descartada sem nunca ter assinado um release real; foi gerada uma nova em 2026-09-13.
- O workflow interpolava `${{ secrets.GPG_SIGNING_KEY_PASSPHRASE }}` direto numa string de shell
  — uma senha com `$` era reinterpretada pelo bash antes do gpg recebê-la, causando "Bad
  passphrase" só em CI (nunca localmente). Corrigido passando secrets via `env:` + `"$VAR"`.

### Critérios de aceite
- [x] Chave de assinatura documentada (`docs/contributing/RELEASE_SIGNING.md`).
- [x] Chave pública incorporada/verificável (`docs/keys/protondrive4linux-release.asc`, chave
      mestra `67A9CE00271C5DE87557AC76BBA104838E512C30`, subchave de assinatura
      `D2D1405C5D85E6268EED6CE2FA5BDA1DD50C980D`).
- [x] Release assinada (`v0.4.2`, verificado com `gpg --verify` contra os 4 artefatos publicados).
- [x] Instruções de verificação (`SECURITY.md#verifying-a-release`).
- [x] CI falha se assinatura não for gerada (uma vez `RELEASE_SIGNING_READY=true`, secret/var
      ausente ou `.asc` faltando derruba o job — não é best-effort).

## M2-002 — Secure updater
**Labels**: `type:security`, `area:security`, `risk:security`
**Status: 🔶 Parcial, e desligado.** Já existe verificação de checksum fail-closed e testada
(`src/updater.rs`: `verify()`, recusa asset sem checksum publicado). Falta a parte de assinatura
(depende de M2-001) — até lá, `updater::UPDATES_ENABLED = false` continua sendo a decisão
correta. **Não reativar antes de M2-001 fechar.**

### Critérios de aceite
- [ ] Release não assinada é recusada.
- [ ] Assinatura inválida é recusada.
- [x] Hash inválido é recusado.
- [x] Repository/owner esperado é fixo (`REPO` constante, corrigida após o rename do repo).
- [x] Nenhum fallback para fonte arbitrária.
- [x] Instalação privilegiada (`pkexec`) só ocorre após validação de checksum — falta ainda
      validação de assinatura antes disso.

## M2-003 — Proton CLI resolution hardening
**Labels**: `type:security`, `area:security`, `risk:security`
**Status: ✅ Fechado.** `proton-drive` é resolvido por PATH (`src/protoncli.rs`, `which`), com
path configurável. Achado real ao implementar: a resolução "segura" (`which()`) só era usada
para diagnóstico (`status`/`doctor`) — a execução de verdade passava o nome puro pro
`Command::new()`, que delega a busca no PATH pro SO (mais permissivo, aceita entradas
relativas). `command_with()` agora resolve pela `which()` hardenizada antes de montar o
`Command`, então o que é exibido é exatamente o que é executado.

### Critérios de aceite
- [x] Definir estratégia segura de resolução: busca em PATH, mas ignora qualquer entrada
      relativa (inclusive `.`/entrada vazia, que o POSIX trata como diretório atual) — o
      truque clássico de PATH injection. Um `binary` configurado explicitamente com `/` é
      honrado como está (é escolha do usuário, não algo que um atacante controla via ambiente).
- [x] Detectar executável inesperado: `binary_location_warning()` avisa se o diretório do
      binário resolvido for gravável por outros usuários — sinal concreto e de baixo
      falso-positivo, ao invés de uma lista de "caminhos esperados" (o CLI é distribuído por
      três pacotes AUR diferentes, `.deb`, `.rpm`, e instalação manual, então uma lista
      branca erraria instalações legítimas).
- [x] Mostrar caminho efetivamente utilizado: `status`/`doctor` já mostravam o path resolvido;
      agora mostram o aviso de localização junto quando aplicável. Superfície na GUI (painel de
      conta) fica como melhoria futura, não bloqueia o fechamento — o caminho já é visível ali.
- [x] Evitar execução acidental de binário malicioso no diretório atual: fechado pelo hardening
      da `which()` acima, combinado com `command_with()` agora realmente usar essa resolução.
- [x] Documentar comportamento: comentários em `which()`/`command_with()`/
      `binary_location_warning()` no código; este arquivo.

## M2-004 — Automated dependency audit
**Labels**: `type:security`, `area:security`
**Status: ✅ Fechado.** CI roda `cargo clippy -- -D warnings`, `cargo fmt --check`, `cargo audit`
(via `rustsec/audit-check`) e agora `cargo deny check` (via `EmbarkStudios/cargo-deny-action`)
em todo push/PR (`.github/workflows/ci.yml`). Config e as exceções documentadas (com motivo) em
`deny.toml` na raiz.

### Critérios de aceite
- [x] Executado em todo PR.
- [x] Vulnerabilidade crítica bloqueia merge.
- [x] Licenças verificadas — `cargo deny check licenses` com allow-list explícita de todas as
      licenças presentes na árvore de dependências (`--all-features`, a mesma que a CI builda).
      Achado real ao configurar: `cargo deny` já detecta e falha por padrão em advisories
      "unmaintained" (`ttf-parser`, `proc-macro-error`), algo que o `cargo audit` atual não fazia
      — ambos ignorados em `deny.toml` com a mesma causa raiz já documentada em `ci.yml`
      (`ttf-parser` via winit/gui, `proc-macro-error` via gtk 0.18/tray-icon).
- [x] Dependências não mantidas identificadas automaticamente — resolvido pelo mesmo
      `cargo deny check advisories` acima (não precisou de ferramenta separada).

## M2-005 — Reproducible release metadata
**Labels**: `type:security`, `area:distribution`
**Status: ✅ Fechado.** `build.rs` (novo, sem dependências de terceiros — só `std::process::Command`)
captura `git rev-parse --short=12 HEAD` em tempo de build e expõe via `PROTONDRIVE_BUILD_SHA`;
`--version` agora mostra `<versão> (<sha>[-dirty])`, ex. `0.4.2 (c499d009e67d)`. `status`/`doctor`
usam a mesma string. Working tree sujo no momento do build soma um sufixo `-dirty` (prática comum
em `rustc`/`cargo`), então um build de dev nunca é confundido com o build de release do mesmo commit.

Dois casos sem `.git` disponível, cobertos:
- **Tarball de source** (gerado via `git archive` no `release.yml`, que exclui `.git` por
  natureza): rebuilder a partir dele cai em `"unknown"` em vez de falhar o build.
- **Empacotadores/CI que buildam fora de um checkout git** (ex. AUR/`.deb`/`.rpm` a partir de um
  tarball extraído): podem fixar o SHA explicitamente via `PROTONDRIVE_BUILD_SHA=<sha> cargo build`,
  sem precisar de `.git`.

Os builds reais de `.deb`/`.rpm`/tarball binário em CI partem de um `actions/checkout@v4` normal
(`.git` presente), então pegam o SHA real automaticamente — nenhuma mudança em `release.yml` foi
necessária.

### Critérios de aceite
- [x] Versão identificável (`Cargo.toml`, tag).
- [x] Commit SHA associado embutido no binário (`build.rs` + `PROTONDRIVE_BUILD_SHA`, usado em
      `--version`/`status`/`doctor`).
- [x] Build metadata — sufixo `-dirty` quando o working tree não está limpo no momento do build.
- [x] Checksums (SHA-256 por asset, publicado pelo GitHub Release).
- [x] Artefatos rastreáveis ao commit de forma independente do changelog — `--version` já basta,
      sem precisar abrir o `CHANGELOG.md` pra saber de qual commit um binário específico veio.

## M2 — Definition of Done
- [x] Release assinada (bloqueia reativação do updater — M2-002 ainda depende disso ficar
      testado em produção antes de reativar).
- [x] `cargo deny check licenses` em CI.
- [x] CLI resolution hardening documentado e implementado.
- [x] Commit SHA no `--version`.
