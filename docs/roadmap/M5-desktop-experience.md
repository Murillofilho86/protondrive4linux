# M5 — Desktop Experience

Funde as antigas "Fase 3" (GUI) e "Fase 4" (integração com desktop) do roadmap pré-M0-M9.

## M5-001 — Sistema de temas (System/Light/Dark)
**Labels**: `type:feature`, `area:gui`
**Status: ✅ Concluído.** Settings → Appearance → Theme: **System** (padrão), **Light** ou
**Dark** (`theme = "system"|"light"|"dark"` no config).

### Como ficou
- **Tokens, não constantes.** As 13 cores que eram `const` espalhadas por ~190 usos viraram campos
  semânticos de um `Palette` (`nav_bg`, `panel`, `panel2`, `sel`, `text`, `dim`, `dim2`, `accent*`,
  `ok`, `danger`, `warn`, mais `line`/`hover`, que são translúcidos pra funcionar sobre qualquer
  superfície), lidos por `th()`. Duas paletas: a escura (a de sempre, derivada dos pixels do
  logo) e uma clara que mantém os mesmos matizes sobre uma base lavanda e escurece as cores de
  status até lerem sobre ela. Achados do auditoria de papéis, que um `s/TEXT/…/` cego teria
  quebrado no modo claro: "texto sobre o destaque" (botão primário) precisa de um token próprio
  (`on_accent`, branco nos dois temas), o knob/track do switch sumiam sobre o cartão branco, e a
  pílula do contador de conflitos usa um âmbar fixo com tinta escura.
- **"System" de verdade.** O winit não informa o tema do sistema no Wayland (`egui` vê `None`),
  então confiar nele faria "System" significar "sempre escuro". `src/appearance.rs` lê
  `org.freedesktop.appearance` / `color-scheme` do portal de configurações (GNOME, KDE, COSMIC…
  todos têm) numa thread, a cada 3 s — então mudar o tema do desktop com o app aberto vale em
  segundos. Sem portal (WM mínimo), cai no que o egui enxerga e, por fim, no escuro.
- **Testes.** Contraste WCAG nas duas paletas (texto ≥ 7:1, texto secundário ≥ 4,5:1, cores de
  status ≥ 3:1 em toda superfície, texto branco sobre cada tom de destaque ≥ 4,5:1); a leitura do
  portal contra um portal falso num `dbus-daemon` privado e **hermético** (sem diretórios de
  serviço — o que também fecha um furo dos testes de notificação: um `dbus-daemon` padrão ativa o
  serviço real); round trip do config. Verificado visualmente nas duas paletas com a captura
  interna do egui (só a janela do app, nunca a tela do usuário), e "System" resolveu pra escuro
  neste desktop, que reporta `color-scheme = 1`.

### Critérios de aceite
- [x] System theme.
- [x] Light.
- [x] Dark.
- [x] Persistência (`theme` no config).
- [x] Hot reload: trocar o seletor vale no quadro seguinte; "System" acompanha o desktop.
- [x] GUI inteira usa tokens de tema (nenhuma `Color32` literal de paleta sobrou fora de
      `light_palette`/`dark_palette` e do selo âmbar do contador).

## M5-002 — Temas customizados
**Status: ✅ Concluído.** Dracula, Nord e Tokyo Night vêm embutidos, e o usuário adiciona os
seus como um arquivo TOML em `~/.config/protondrive4linux/themes/` — sem tocar em código de
componente. Formato e tokens em `docs/THEMES.md`.

### Como ficou
- **Temas são dados** (`src/themes.rs`, sem dependência de GUI): as duas paletas embutidas, o
  parser e o catálogo. Cada cor é opcional — um tema declara só `name` e `base` (`light|dark`) e
  sobrescreve o que quiser; o resto vem da base. A `base` também escolhe o visual padrão dos
  widgets do egui. Os três presets são arquivos de verdade em `assets/themes/` (compilados com
  `include_str!`), então servem de exemplo pra copiar.
- **Erros que ajudam, não que escondem.** `deny_unknown_fields`: `acent = "#fff"` vira
  `line 4: unknown field 'acent', expected one of …` na página de Settings, não um erro
  silenciado. Um arquivo ruim nunca esconde os outros; nome reservado (`system|light|dark`),
  arquivo gigante e cor inválida são recusados com o motivo.
- **Aviso de legibilidade, não recusa.** Tema ilegível (contraste WCAG: texto < 4,5:1, secundário
  < 3:1, `on_accent` < 3:1) ganha um aviso em Settings — o tema é do usuário — enquanto os
  embutidos são cobrados com régua mais alta nos testes (texto 7:1, secundário 4,5:1, status 3:1
  em toda superfície, `on_accent` 4,5:1 em todo tom de destaque). Isso pegou um erro de verdade:
  o vermelho do Nord oficial dava 2,66:1 sobre a seleção; o preset usa um mais claro.
- **O config não quebra por causa de um tema.** `theme = "Nome"` é guardado como foi escrito;
  se o arquivo sumiu, o app abre com o tema do sistema e avisa — um tema apagado não pode impedir
  o app de iniciar. Arquivos são relidos enquanto Settings está aberto (sem reiniciar), e há um
  botão *Folder* que cria e abre a pasta.
- A régua de contraste e as duas paletas saíram do `gui.rs` pra `themes.rs`: um lugar só, testável
  sem GUI, e o mesmo teste vale pros temas embutidos e pros presets.

## M5-003 — File status indicators
**Status: ⬜ Não iniciado.** Estados: `Synced, Syncing, Conflict, Error, Offline, Excluded,
Pending`.

## M5-004 — File manager integration
**Status: ⬜ Não iniciado.** Prioridade: Nautilus → Dolphin → Nemo → Thunar.

> **Nota de arquitetura**: isto é um atalho/bookmark para a pasta local sincronizada, **não** um
> mount virtual — não depende de FUSE (ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md`).

### Critérios de aceite
- [ ] Ícone/status por arquivo/pasta.
- [ ] Context menu.
- [ ] Open local folder.
- [ ] Sync status.

## M5-005 — Onboarding, tray, notificações desktop
**Status: ✅ Concluído.** A maior parte já existia no código — o status "não iniciado" estava
desatualizado — e o que faltava de verdade era o ícone de estados e as notificações nativas;
os dois estão prontos.

- [x] GUI egui com onboarding (porta de login quando o CLI falta/não há sessão), lista de pares
      de pasta, status de sync por pasta e log de atividade em tempo real.
- [x] Ícone de bandeja com estados visuais claros: um selo no canto do logo — check verde
      (sincronizado), reticências azuis (sincronizando), barras cinza (auto sync desligado),
      "!" vermelho (erro, ou arquivos esperando retry), traço âmbar (deslogado) — cada um com
      glifo próprio, pra não depender só de distinguir cores. O menu ganha uma linha de status
      no topo (`Syncing docs (3/10)`, `Up to date · synced 2 minutes ago`, ...). Desenhado do
      zero em `src/icon.rs`, sem dependência nova; o estado agregado vem de
      `AppState::overall()` (`src/service.rs`), reutilizável por outros frontends. Verificado
      ao vivo no painel do COSMIC (sincronizado, erro de upload e deslogado).
- [x] Fluxo de "escolher pastas para sincronizar" (seletor de exclusão).
- [x] Notificações desktop nativas para conflitos e erros (`src/notify.rs`). Uma única chamada
      `org.freedesktop.Notifications.Notify` pelo barramento de sessão — todo daemon de
      notificação implementa, então vale pra GNOME/KDE/COSMIC/XFCE/mako/dunst — via `zbus`, que
      **já estava no build** (a camada de acessibilidade do eframe puxa); a única mudança no
      `Cargo.lock` é uma linha. O que decide *quando* avisar é uma máquina de estados pura
      (`Notifier`, relógio injetado, testada): um problema de sync só avisa depois de durar ~90s
      (um blip de rate-limit que some no primeiro retry fica quieto), uma vez por episódio e no
      máximo a cada 10 min; conflitos que chegam juntos viram uma notificação só, espaçadas de
      ≥30s (o excedente espera e entra na próxima, nada se perde); deslogar avisa na transição,
      não enquanto dura. Só conflitos que acontecem *depois* do daemon subir contam (reiniciar
      nunca repete os antigos). Vem do daemon da bandeja (precisa de "Run in system tray");
      switch "Desktop notifications" em Settings / `notifications = false`. Verificado de ponta a
      ponta: teste automatizado com um `dbus-daemon` privado + servidor de notificações falso
      (prova o formato exato da chamada, que de outro modo falharia em silêncio), e o daemon
      real mandando a chamada ao daemon de notificações do COSMIC.

Fica de fora, de propósito: **clicar na notificação** pra abrir o app (precisa ouvir
`ActionInvoked` numa conexão persistente; `--page conflicts` já existe pra isso) e notificações
do `watch` headless (o binário CLI não compila `zbus`; só o daemon da GUI notifica).

Achado ao fazer o ícone: depois do M4-004, uma execução que só *pula* arquivos em backoff
termina com zero erros, então o par aparecia como "Synced"/"watching" mesmo com arquivos
falhando. `PairState::retrying` passou a contar esses arquivos e a banner de Activity, a linha
de cada pasta e o ícone da bandeja agora mostram "N arquivos esperando retry".

## M5 — Definition of Done
Um usuário novo instala via AUR, loga, escolhe uma pasta, e vê sync funcionando sem tocar em
terminal.
