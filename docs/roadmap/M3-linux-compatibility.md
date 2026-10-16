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
**Status: ✅ Fechado. Decisão: Flatpak não é viável hoje — nem como `GUI only`, nem como
`GUI + sync daemon`.** Ambos os formatos dependem do mesmo requisito, e esse requisito é
bloqueado por um fator fora do nosso controle.

### O bloqueador: o `proton-drive` CLI não tem distribuição sandbox-friendly

protondrive4linux é, por design (ver `CLAUDE.md`), um wrapper fino sobre o `proton-drive` CLI
oficial — todo login, autenticação e acesso à API da Proton passam por ele (`protoncli.rs`
resolve `proton-drive` via `$PATH`, nunca reimplementa o protocolo). Qualquer formato de Flatpak
precisa alcançar esse binário, inclusive um `GUI only` (login/status já dependem dele).

Levantamento sobre o próprio `proton-drive` CLI:
- É um binário de terceiro, não vendorizado neste repo. Upstream é TypeScript empacotado com
  Bun; a AUR tem três variantes — `proton-drive-cli-bin` (binário precompilado, **licença
  proprietária**, ~100MB self-contained), `proton-drive-cli` (builda da fonte, MIT, mas mantém
  `bun` como dependência de runtime pra ficar em ~12MB), `proton-drive-cli-git`.
- Não existe Flatpak, AppImage ou Snap oficial da Proton pra ele — só os binários Linux
  avulsos (x64/arm64) e os pacotes da comunidade.
- Ele mesmo guarda a sessão de login via `libsecret`/Secret Service — ou seja, mesmo se
  conseguíssemos invocá-lo de dentro de um sandbox, ele teria sua própria necessidade de
  `--talk-name=org.freedesktop.secrets`, independente da nossa.

Dentro de um sandbox Flatpak, as únicas duas formas de alcançar esse binário são:
1. **`flatpak-spawn --host`** (`--talk-name=org.freedesktop.Flatpak`): documentado pelo próprio
   Flatpak como acesso que permite rodar qualquer comando no host com os privilégios do
   usuário — na prática, anula o sandbox por completo. Historicamente tratado como bandeira
   vermelha em revisão do Flathub (casos documentados de apps como o Atom sendo citados como
   exemplo do que não fazer); viável só pra distribuição fora do Flathub (repo próprio, `.flatpak`
   direto).
2. **Embutir o binário no manifest**: barrado pela licença proprietária da variante `-bin`; a
   variante MIT exige trazer o runtime `bun` inteiro pro sandbox (viola o princípio de "CLI
   dependency-light" do projeto) e, mesmo assim, não reduz nenhum dos acessos amplos abaixo —
   o binário embutido ainda precisaria do mesmo filesystem/keyring que o processo seria capaz
   de acessar de qualquer forma.

### Outros achados (relevantes se isso for revisitado no futuro)

- **Filesystem (sync de pastas arbitrárias)**: o Document Portal (FUSE por arquivo, escolhido via
  `FileChooser`) não serve pra um daemon que faz `inotify` + leitura/escrita direta e contínua.
  O padrão real usado por apps comparáveis (ex.: Dropbox Flatpak) é a permissão estática
  `--filesystem=home`, que a própria documentação do Flatpak orienta a evitar quando possível —
  aceitável como precedente, mas não elimina o bloqueador do CLI acima.
- **Tray (ícone na bandeja)**: viável em princípio via StatusNotifierItem
  (`--talk-name=org.kde.StatusNotifierWatcher`), mas crates como a usada aqui (`tray_icon`,
  mesma família do `ksni`) historicamente tentam registrar um nome `org.kde.StatusNotifierItem-
  $PID-$ID` — dentro do sandbox o PID costuma colidir (sempre baixo, isolado no próprio pid
  namespace), então precisaria registrar pelo nome único da conexão D-Bus (é o que Electron/Qt
  já fazem hoje) em vez do nome baseado em PID.
- **Keyring**: a sessão de login do `proton-drive` CLI via `libsecret` teria que passar pelo
  Secret Service (`--talk-name=org.freedesktop.secrets`) — há relatos reais de persistência
  quebrada (secret não sobrevive a um restart do app) mesmo com a permissão concedida.
- **Background + autostart**: o mecanismo correto dentro do sandbox é o portal
  `org.freedesktop.portal.Background` (`RequestBackground`, com a opção `autostart`) — não dá
  pra simplesmente escrever em `~/.config/autostart/*.desktop` como o `set_autostart()` atual
  faz (`src/bin/gui.rs`), isso exigiria reescrever essa função pra chamar o portal. Há relatos
  de bugs reais de autostart via portal não disparando mesmo após retornar sucesso, e um CVE
  antigo (CVE-2024-32462) de sandbox escape via esse mesmo portal (corrigido em
  xdg-desktop-portal ≥ 1.18.4).

### Critério de aceite
- [x] Filesystem permissions investigado.
- [x] Portals (FileChooser/Document, Background, Secret) investigados.
- [x] Background services / autostart investigado.
- [x] Tray investigado.
- [x] Keyring investigado.
- [x] Acesso ao `proton-drive` CLI de dentro do sandbox investigado — **é o bloqueador real**.
- [x] Documento de decisão publicado (esta seção): **Flatpak não é viável hoje**, por uma
      dependência externa fora do nosso controle. M7-001 (`M7-distribution.md`) permanece
      bloqueado indefinidamente; reabrir só se a Proton passar a distribuir o CLI de forma
      compatível com sandbox (Flatpak/AppImage oficial próprio, ou variante MIT sem a
      dependência de runtime do `bun`).
