# M3 — Linux Compatibility

**Objetivo**: expandir suporte sem fragmentar o código.

## M3-001 — Linux compatibility matrix
**Labels**: `type:documentation`, `area:packaging`
**Status: 🔶 Parcial.** A coluna **Package** (instalação limpa do `.deb`/`.rpm`, ver M3-003/M3-004)
está coberta de ponta a ponta, testada em container Docker limpo:

| Distro | glibc | `.deb`/`.rpm` instala? |
| --- | --- | --- |
| Ubuntu 22.04 LTS | 2.35 | ✅ |
| Ubuntu 24.04 | 2.39 | ✅ |
| Debian 12 | 2.36 | ✅ |
| Debian 13 | 2.41 | ✅ |
| RHEL 9 / Rocky Linux 9 / AlmaLinux 9 | 2.34 | ✅ |
| Fedora (atual) | — | ✅ |
| Arch | — | ✅ (ver M3-002, `PKGBUILD`) |

As colunas **Sync, GUI, Tray, Notifications, File manager** continuam sem teste real — isso
exige um desktop de verdade (GNOME/KDE/XFCE/COSMIC/Hyprland) e, pra Sync especificamente, uma
conta Proton real; nenhum dos dois existe num container efêmero/headless.

### Critério de aceite
Matriz publicada com colunas: Sync, GUI, Tray, Notifications, File manager, systemd, Package.
**Package** ✅; as demais colunas seguem pendentes (ver nota acima).

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
**Status: ✅ Fechado.** Testado em container Docker limpo: instalação (`apt-get install -y
./pkg.deb`), upgrade (`v0.4.2` → `v0.4.3`), uninstall (`apt-get remove`) — todos limpos, sem
arquivo órfão. `systemd`/desktop entry/ícones presentes e corretos.

Achado real (ver M3-004/`release.yml` pro detalhe completo): o `.deb` publicado até aqui era
buildado em `ubuntu-latest` (glibc 2.39), o que o deixava **inválido em Ubuntu 22.04 LTS e
Debian 12** — ambos ainda com suporte ativo. `release.yml` agora builda o binário dentro de um
container RockyLinux 9 (glibc 2.34, o piso mais baixo entre os alvos) e empacota o `.deb` de
volta num runner Ubuntu 22.04 (precisa do `dpkg-shlibdeps` de lá pra calcular `Depends:`
corretamente) — `Depends: libc6 (>= 2.34), ...` agora cobre todos os alvos testados.

### Critérios de aceite
- [x] `.deb` gerado (`cargo deb --features gui`).
- [x] Instalação limpa (Ubuntu 22.04, Ubuntu 24.04, Debian 12, Debian 13 — todas testadas em
      container Docker).
- [x] Uninstall limpo.
- [x] Upgrade.
- [x] systemd.
- [x] Desktop entry.

## M3-004 — Pacote RPM
**Status: ✅ Fechado.** Testado em container Docker limpo: instalação, upgrade (`v0.4.2` →
`v0.4.3`), uninstall — todos limpos, em Fedora e em Rocky Linux 9 (representativo da família
RHEL 9).

Dois achados reais ao fechar isso, ambos documentados com o racional completo em
`.github/workflows/release.yml`:
- O mesmo piso de glibc do M3-003 (binário buildado em `ubuntu-latest` exigia `GLIBC_2.39`,
  inválido em toda a família RHEL 9, que tem glibc 2.34) — mesma correção (build em container
  RockyLinux 9).
- `cargo-generate-rpm` só calcula `Requires:` corretamente se o pacote `rpm-build` (que fornece
  `/usr/lib/rpm/find-requires`) estiver instalado no ambiente de build; sem ele, cai num modo
  "builtin" que gera entradas como `libc.so.6(GLIBC_2.18)[WEAK](64bit)` — um sufixo `[WEAK]`
  literal que nenhum pacote real jamais fornece, tornando o `.rpm` **impossível de instalar em
  qualquer sistema**. `release.yml` agora instala `rpm-build` antes de empacotar.

### Critérios de aceite
- [x] Fedora.
- [x] RHEL-compatible (Rocky Linux 9, testado — glibc 2.34 como piso).
- [x] Upgrade.
- [x] Uninstall.
- [x] systemd.

## M3-005 — Pesquisa de viabilidade Flatpak
**Labels**: `type:research`, `area:distribution`, `priority:high`
**Status: ⬜ Não iniciado.** Investigar antes de qualquer código: filesystem permissions,
portals, background services, tray, keyring, acesso ao `proton-drive` CLI de dentro do sandbox,
autostart.

### Critério de aceite
Documento de decisão: Flatpak será `GUI only` ou `GUI + sync daemon`. Bloqueia M7-001 (Flatpak
em `M7-distribution.md`) até esta decisão existir.
