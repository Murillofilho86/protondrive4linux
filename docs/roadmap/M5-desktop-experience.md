# M5 — Desktop Experience

Funde as antigas "Fase 3" (GUI) e "Fase 4" (integração com desktop) do roadmap pré-M0-M9.

## M5-001 — Sistema de temas (System/Light/Dark)
**Labels**: `type:feature`, `area:gui`
**Status: ⬜ Não iniciado.**

### Arquitetura proposta
```text
Theme
 ├── colors
 ├── typography
 ├── spacing
 └── components
```

### Critérios de aceite
- [ ] System theme.
- [ ] Light.
- [ ] Dark.
- [ ] Persistência.
- [ ] Hot reload quando possível.
- [ ] GUI inteira usa tokens de tema.

## M5-002 — Temas customizados
**Status: ⬜ Não iniciado.** Não começar antes de M5-001 ter os tokens de tema estáveis.
Exemplos-alvo: Dracula, Nord, Tokyo Night. O usuário deve poder adicionar temas sem modificar o
código dos componentes.

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
