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
**Status: 🔶 Parcial.** Overlap direto com M1 (disaster suite, offline recovery). Não é uma
issue nova — é o checkpoint de "M1 realmente fechou" antes do release candidate. Cenários:
`kill -9`, power loss, network loss, disk full, permission denied, remote unavailable, local
unavailable, conflict, rename, large file, large tree.

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
