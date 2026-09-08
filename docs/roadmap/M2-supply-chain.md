# M2 — Supply Chain Security

**Objetivo**: garantir que o binário distribuído é exatamente o artefato que o projeto produziu.

## M2-001 — Release signing
**Labels**: `type:security`, `area:security`, `priority:critical`
**Status: ⬜ Não iniciado — gap real, já sinalizado no `SECURITY_AUDIT.md`.** Não há assinatura
criptográfica de release hoje, só o checksum SHA-256 do próprio GitHub (mesmo canal do artefato,
não é verificação independente). **Este é o item de segurança mais importante ainda aberto.**

### Critérios de aceite
- [ ] Chave de assinatura documentada.
- [ ] Chave pública incorporada/verificável.
- [ ] Release assinada.
- [ ] Instruções de verificação.
- [ ] CI falha se assinatura não for gerada.

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
**Status: ⬜ Não iniciado — gap real.** `proton-drive` é resolvido por PATH hoje
(`src/protoncli.rs`, `which`), com path configurável.

### Critérios de aceite
- [ ] Definir estratégia segura de resolução.
- [ ] Detectar executável inesperado.
- [ ] Mostrar caminho efetivamente utilizado (expandir `doctor` — ver M6-003).
- [ ] Evitar execução acidental de binário malicioso encontrado no diretório atual.
- [ ] Documentar comportamento.

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
- [ ] Release assinada (bloqueia reativação do updater).
- [ ] `cargo deny check licenses` em CI.
- [ ] CLI resolution hardening documentado e implementado.
- [ ] Commit SHA no `--version`.
