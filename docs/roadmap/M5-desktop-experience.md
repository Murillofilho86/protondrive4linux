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
**Status: ⬜ Não iniciado.**

- [ ] Revisar/expandir a GUI egui existente: onboarding (login), lista de pares de pasta, status
      de sync por pasta, log de atividade em tempo real.
- [ ] Ícone de bandeja com estados visuais claros (sincronizado/sincronizando/pausado/erro).
- [ ] Fluxo de "escolher pastas para sincronizar" (seletor de exclusão).
- [ ] Notificações desktop nativas (`notify-rust`) para conflitos e erros.

## M5 — Definition of Done
Um usuário novo instala via AUR, loga, escolhe uma pasta, e vê sync funcionando sem tocar em
terminal.
