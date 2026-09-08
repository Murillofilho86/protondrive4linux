# M7 — Distribution

## M7-001 — Flatpak
**Status: 🔒 Bloqueado por M3-005.** Não implementar antes da pesquisa de viabilidade concluir.

### Critérios de aceite (quando desbloqueado)
- [ ] Manifest.
- [ ] Build reproduzível.
- [ ] Permissions mínimas.
- [ ] GUI.
- [ ] Sync daemon.
- [ ] Autostart.
- [ ] Keyring.
- [ ] File access.
- [ ] Update mechanism.

## M7-002 — Instalador universal (`.sh`)
**Status: ⬜ Não iniciado.** Nunca `curl | bash`.

### Fluxo obrigatório
```text
download → verify signature → detect platform → install → validate
```
Depende de M2-001 (release signing) para a etapa de verificação.

## M7-003 — `protondrive4linux doctor` (expandir)
**Status: 🔶 Parcial.** `doctor` já existe (`src/main.rs: cmd_doctor`), mas hoje só resolve o
binário do CLI, mostra a versão, e faz probe de `filesystem list` numa pasta — é uma ferramenta
de debug do adapter, não o diagnóstico de instalação completo.

### Critérios de aceite
- [x] Proton CLI (resolução + versão).
- [ ] Credentials (presença, não conteúdo).
- [ ] Filesystem (permissões de `state_dir`/config — devem ser `0700`/`0600`).
- [ ] systemd (unit ativa?).
- [ ] Desktop entry presente.
- [ ] Network (conectividade básica).
- [ ] Version (do próprio binário, não só do CLI — depende de M2-005).

## M7-004 — Publicação real na AUR
**Status: 🔒 Bloqueado externamente.** O job `aur-publish` já existe em `release.yml`
(`aur-publish`, gatilho em toda tag `v*`), mas está com `if: vars.AUR_ACCOUNT_READY == 'true'`
(hoje `false`) — a AUR não aceita registro de conta nova no momento (incidente de pacotes
órfãos). Sem ação possível do lado do fork além de esperar e configurar os secrets
(`AUR_USERNAME`, `AUR_EMAIL`, `AUR_SSH_PRIVATE_KEY`) quando a conta existir.

### Critérios de aceite
- [x] Job de CI implementado e testado (`aur-publish` em `.github/workflows/release.yml`).
- [ ] `AUR_ACCOUNT_READY=true` + secrets configurados.
- [ ] Primeira publicação real confirmada no AUR.

## M7-005 — `.SRCINFO` e validação `namcap`
**Status: ⬜ Não iniciado como checklist formal.** `PKGBUILD` já existe e builda limpo em
container (M3-002); falta rodar `namcap` explicitamente e versionar o `.SRCINFO` gerado.
