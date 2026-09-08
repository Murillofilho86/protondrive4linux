# M3 — Linux Compatibility

**Objetivo**: expandir suporte sem fragmentar o código.

## M3-001 — Linux compatibility matrix
**Labels**: `type:documentation`, `area:packaging`
**Status: ⬜ Não iniciado.** Só Arch foi validado (em container limpo, ver M3-002). Ubuntu,
Debian, Fedora, openSUSE × GNOME/KDE/XFCE/COSMIC/Hyprland: nada testado ainda.

### Critério de aceite
Matriz publicada com colunas: Sync, GUI, Tray, Notifications, File manager, systemd, Package.

## M3-002 — Arch package
**Labels**: `area:packaging`, `area:distribution`
**Status: 🔶 Quase concluído.** `PKGBUILD` existe, `makepkg -s` validado de ponta a ponta em
container Arch limpo (build + 61 testes + empacotamento).

### Critérios de aceite
- [x] PKGBUILD reproduzível.
- [x] Dependências corretas (`proton-drive-cli` dependência direta, `glib2` explícito).
- [x] systemd user service.
- [x] Desktop entry.
- [x] GUI e CLI empacotados.
- [x] Testado em Arch limpo (build).
- [ ] **Pendência real, manual**: `makepkg -si` (instalação de verdade) + `login` + `sync
      --dry-run` com conta Proton real — precisa de máquina real, não container efêmero.

## M3-003 — Pacote Debian (`.deb`)
**Status: ⬜ Não iniciado.**

### Critérios de aceite
- [ ] `.deb` gerado (`cargo deb --features gui` já configurado em `Cargo.toml`, falta validar
      instalação limpa).
- [ ] Instalação limpa.
- [ ] Uninstall limpo.
- [ ] Upgrade.
- [ ] systemd.
- [ ] Desktop entry.

## M3-004 — Pacote RPM
**Status: ⬜ Não iniciado.**

### Critérios de aceite
- [ ] Fedora.
- [ ] RHEL-compatible.
- [ ] Upgrade.
- [ ] Uninstall.
- [ ] systemd.

## M3-005 — Pesquisa de viabilidade Flatpak
**Labels**: `type:research`, `area:distribution`, `priority:high`
**Status: ⬜ Não iniciado.** Investigar antes de qualquer código: filesystem permissions,
portals, background services, tray, keyring, acesso ao `proton-drive` CLI de dentro do sandbox,
autostart.

### Critério de aceite
Documento de decisão: Flatpak será `GUI only` ou `GUI + sync daemon`. Bloqueia M7-001 (Flatpak
em `M7-distribution.md`) até esta decisão existir.
