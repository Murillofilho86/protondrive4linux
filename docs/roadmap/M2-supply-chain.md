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
**Status: 🔶 Quase concluído.** CI já roda `cargo clippy -- -D warnings`, `cargo fmt --check` e
`cargo audit` (via `rustsec/audit-check`) em todo push/PR (`.github/workflows/ci.yml`).

### Critérios de aceite
- [x] Executado em todo PR.
- [x] Vulnerabilidade crítica bloqueia merge.
- [ ] Licenças verificadas — falta `cargo deny check licenses` automatizado em CI (hoje só as
      advisories de segurança são checadas; licença/manutenção foi feita manualmente na
      auditoria original, não está em CI).
- [ ] Dependências não mantidas identificadas automaticamente.

## M2-005 — Reproducible release metadata
**Labels**: `type:security`, `area:distribution`
**Status: 🔶 Parcial.** `scripts/release.sh` já recusa tag sem seção de changelog e valida a
versão do `Cargo.toml` contra a tag.

### Critérios de aceite
- [x] Versão identificável (`Cargo.toml`, tag).
- [ ] Commit SHA associado embutido no binário (`--version` hoje não mostra de qual commit veio
      o build).
- [ ] Build metadata.
- [x] Checksums (SHA-256 por asset, publicado pelo GitHub Release).
- [ ] Artefatos rastreáveis ao commit de forma independente do changelog.

## M2 — Definition of Done
- [x] Release assinada (bloqueia reativação do updater — M2-002 ainda depende disso ficar
      testado em produção antes de reativar).
- [ ] `cargo deny check licenses` em CI.
- [x] CLI resolution hardening documentado e implementado.
- [ ] Commit SHA no `--version`.
