# Roadmap — Proton Drive Client Nativo para Arch Linux (fork de NeutronSync)

> Documento de trabalho para uso com Claude Code. Cada fase é uma unidade de trabalho
> independente; feche uma fase (com os critérios de saída atendidos) antes de abrir a próxima.

## Contexto e decisões já tomadas

- **Base do fork**: [`WilhelmZA/protondrive_linux_sync`](https://github.com/WilhelmZA/protondrive_linux_sync) ("NeutronSync"), branch `rust`.
- **Motor oficial**: todo o acesso ao Proton Drive passa pelo **`proton-drive` CLI oficial** da Proton
  (mantido por eles, builds em `proton.me/download/drive/cli`). O fork **não** deve reimplementar
  autenticação nem criptografia por conta própria — isso é delegado ao binário oficial.
- **Por que essa base**: é o único projeto candidato com engine de sync explícito (baseline
  three-way-merge), GUI nativa (egui) já esboçada, e nenhuma dependência de rclone ou de SDKs
  não oficiais.
- **Trade-off aceito conscientemente**: por não usar rclone/FUSE, o produto de curto prazo é
  "pasta local sincronizada" (modelo Dropbox clássico), não um filesystem montado sob demanda.
  Mount FUSE fica como fase avançada opcional (Fase 6), mantendo fidelidade a "só libs oficiais"
  ao construir o FUSE como camada fina sobre o CLI oficial, nunca via rclone.
- **Alvo de empacotamento**: Arch Linux / AUR como distribuição primária.

## Princípio orientador de todas as fases

Este código-fonte vem de um repositório pessoal, recém-criado, sem histórico de auditoria por
terceiros, sem contribuidores externos, e sem uso em produção comprovado. **Trate-o como código
não confiável até prova em contrário** — isso vale mesmo que os testes automatizados do próprio
projeto passem. A Fase 0 existe para estabelecer essa prova antes de qualquer investimento em
features novas.

---

## Fase 0 — Auditoria de Segurança e Due Diligence do Código Herdado

**Objetivo**: decidir, com evidência, o que do NeutronSync é seguro manter, o que precisa reescrita,
e o que deve ser descartado, antes de construir qualquer coisa em cima dele.

### 0.1 Proveniência e integridade da cadeia de build
- [x] Verificar autoria dos commits (`git log --format='%an %ae %ad'`) — checar consistência de
      identidade, saltos temporais suspeitos (muitos commits/releases no mesmo dia), e se há GPG
      signing real nas tags (o README alega "signed releases" — confirmar, não assumir).
- [x] Auditar `Cargo.toml` / `Cargo.lock` por completo: toda dependência third-party, versão fixada,
      data de publicação no crates.io, número de downloads, maintainer. Marcar qualquer crate com
      poucos downloads/manutenção recente como candidato a substituição.
- [x] Rodar `cargo audit` e `cargo deny check` contra o `Cargo.lock` herdado.
- [x] Verificar se o binário `proton-drive` é sempre resolvido via PATH/checksum documentado, nunca
      baixado automaticamente sem verificação pelo próprio fork (evitar supply-chain attack via
      download silencioso).

### 0.2 Superfície de execução de processo externo
- [x] Mapear **todo** ponto do código que faz `std::process::Command` (ou equivalente) para invocar
      o `proton-drive` CLI. Esse é o ponto de maior risco: injeção de argumento, path hijacking
      (binário `proton-drive` errado sendo executado do PATH), variáveis de ambiente vazando para
      o subprocesso.
- [x] Confirmar que argumentos passados ao CLI nunca são construídos por concatenação de string a
      partir de input do usuário/config sem sanitização (paths de pastas configuráveis pelo usuário
      são o vetor óbvio).
- [x] Confirmar que segredos (senha, mailbox password, 2FA) nunca passam por argumento de processo
      (visível via `/proc/<pid>/cmdline` para outros usuários) — devem ir via stdin ou arquivo com
      permissão restrita, como o próprio CLI oficial recomenda.

### 0.3 Armazenamento de credenciais e estado
- [x] Confirmar que o fork **não** duplica nem faz cache de sessão fora do que o `proton-drive`
      CLI já gerencia (que fica no keyring do SO). Qualquer cache próprio de token é superfície
      de ataque adicional que o projeto original pode ou não ter tratado corretamente.
- [x] Revisar permissões de arquivo de tudo que o fork escreve em disco: config TOML, baseline
      de sync, logs. Confirmar `0600`/`0700` onde aplicável (dados de caminho de arquivo do
      usuário podem ser sensíveis). — corrigido: `state_dir` e o config TOML agora são `0700`/`0600`.
- [x] Verificar tratamento de `PROTON_DRIVE_UNSAFE_SECRETS` e modos de fallback sem keyring —
      confirmar que o fork nunca ativa esse modo silenciosamente.

### 0.4 Modelo de sync — corretude e segurança de dados
- [x] Ler `docs/SYNC_MODEL.md` do projeto original linha a linha e validar cada alegação contra
      o código real (não confiar na documentação por si só).
- [x] Escrever testes adversariais próprios para os cenários que o README alega tratar: baseline
      ausente, pasta local desmontada, pasta remota que sumiu, falha no meio da transferência —
      cobertos (o gap de cobertura para "pasta remota base sumida" foi fechado com
      `vanished_remote_base_suppresses_delete`). **Pendente**: corrida real entre watch (inotify)
      e rescan periódico sob carga — não reproduzida, exigiria o `proton-drive` CLI real.
- [x] Validar especificamente o caso de **deleção**: simular os cenários descritos e confirmar
      empiricamente que nunca há deleção em massa não intencional — este é o risco mais grave
      de qualquer sync engine (perda de dados do usuário).
- [ ] Avaliar a estratégia de comparação padrão (`size+mtime`) quanto a falsos negativos e decidir
      se o padrão do fork deve ser `sha1` (mais seguro, mais lento) em vez de manter o upstream.
      **Adiado para a Fase 2** (decisão de produto, não bug de segurança).

### 0.5 GUI e superfície de tray/IPC
- [x] Revisar o `docs/GUI_API.md` e a comunicação entre o daemon de watch e a GUI — confirmar que
      não há socket/IPC exposto sem controle de acesso (mesmo em localhost). — é um arquivo
      (`status.json`), não socket; herdava o mesmo problema de permissões do item 0.3, já corrigido.
- [x] Verificar se dependências de GUI (egui, tray via `libayatana-appindicator3`) têm CVEs
      conhecidos nas versões fixadas. — `cargo deny check advisories` cobre as 533 crates do
      lockfile, incluindo as da feature `gui`; nenhuma advisory conhecida.

### 0.6 Auto-update
- [x] O README alega atualização in-app via releases do GitHub com verificação de checksum e
      instalação via `pkexec`. Auditar esse fluxo com atenção redobrada — é o único ponto do
      projeto com potencial de escalação de privilégio. Confirmar: checksum vem de canal
      confiável (não só do mesmo release que está sendo verificado), assinatura GPG é
      efetivamente checada antes do `pkexec`, e não há TOCTOU entre download e instalação. —
      confirmado que o checksum vem do mesmo canal (não é assinatura independente); sem TOCTOU
      explorável (diretório de download `0700` por PID).
- [x] Decisão de design: considerar desabilitar auto-update na v0 do fork e mover para
      atualização apenas via AUR (`pacman -Syu`), eliminando essa superfície de ataque até
      o mecanismo ser reescrito e auditado com calma. — feito: `updater::UPDATES_ENABLED = false`.

### Critério de saída da Fase 0 — ✅ atendido
`SECURITY_AUDIT.md` documenta os achados classificados e a decisão por item. Os 4 itens
Crítico/Alto (REPO do updater apontando pro upstream, permissões de `state_dir`, auto-update
sem assinatura independente, gap de cobertura em `remote_root_missing`) foram corrigidos e
commitados (`6cad543`, `883778a`, `a5e25ff`) antes da Fase 1 começar. Uma revisão de código
adicional (7 achados: índices de diálogo desatualizados, nomes de par duplicados, remoção
local sem `gio`, corrida de lock, etc.) também foi feita e corrigida na mesma janela.

---

## Fase 1 — Fundação do Fork

**Objetivo**: ter o fork rodando localmente, com CI própria, antes de qualquer feature nova.

- [x] Fork real no seu GitHub, rebranding mínimo (nome do binário, app ID, sem usar marca/ícone
      da Proton — ver `NOTICE.md`/trademark do upstream). — renomeado para **protondrive4linux**
      (ver a discussão sobre a política de marca da Proton nesta sessão); repo:
      `Murillofilho86/protondrive_linux_sync`, branch `master`.
- [x] `cargo test` completo passando localmente no Arch Linux (não só CI do upstream). — 61/61
      (22 unitários + 39 em `tests/engine.rs`).
- [x] Confirmar build reprodutível: `cargo build --release` e `cargo build --release --features gui`
      funcionando com as libs do sistema listadas no README (`libgtk-3-dev`, etc. — mapear para
      nomes de pacote Arch: `gtk3`, `libxkbcommon`, `wayland`, `libx11`, `libxcb`, `mesa`,
      `libayatana-appindicator`... confirmar cada um). — confirmado num container `archlinux:base`
      genuinamente limpo (não nesta máquina de dev, que já tinha quase tudo instalado): build,
      testes e empacotamento dos dois binários funcionaram do zero.
- [x] Configurar CI própria (GitHub Actions ou similar) rodando: `cargo test`, `cargo clippy -- -D warnings`,
      `cargo audit`, `cargo fmt --check` a cada PR.
- [x] `PKGBUILD` inicial para build local via `makepkg` (ainda não submeter ao AUR nesta fase). —
      variante `-git`; achou e corrigiu um bug real (`options=('!lto')` — o `-flto=auto` padrão do
      Arch quebra a compilação C bundled do SQLite) que teria impedido a instalação de qualquer
      pessoa usando configuração padrão do `makepkg.conf`.
- [x] Decidir e documentar o novo nome do projeto (evitar confusão com "NeutronSync" upstream e
      com qualquer marca Proton). — **protondrive4linux**.

### Critério de saída — ✅ atendido (com uma ressalva)
`makepkg -s` (build) validado de ponta a ponta num container Arch limpo: clona do GitHub,
`pkgver()` correto a partir da tag `v0.4.0`, compila CLI+GUI, roda os 61 testes, empacota.
**Ressalva**: `makepkg -si` (instalar de verdade) + `login` + `sync --dry-run` com uma conta
Proton real não foi executado nesta sessão — exige credenciais interativas do usuário num
sistema real, não um container efêmero. Fica como o próximo passo manual antes de considerar
a Fase 1 100% fechada.

---

## Fase 2 — Correção e Fortalecimento do Sync Engine

**Objetivo**: aplicar as correções identificadas na Fase 0 e elevar a robustez do engine antes
de adicionar escopo novo.

- [ ] Implementar as correções críticas/altas da auditoria de segurança.
- [ ] Suite de testes de propriedade (`proptest` ou similar) para o classificador de mudanças
      (created/modified/deleted por lado) — gerar sequências aleatórias de operações de
      filesystem e validar que o resultado nunca diverge do esperado.
- [ ] Modo `compare = "sha1"` como padrão do fork (trade-off de performance documentado e
      configurável, mas seguro por padrão).
- [ ] Métricas/logging estruturado (não só texto) para depuração de sync — essencial antes de
      abrir para outros usuários.
- [ ] Tratamento explícito de rate limit da API da Proton (o `proton-drive` CLI provavelmente
      já trata, mas o fork precisa reagir com backoff correto no lado do watch/rescan).

### Critério de saída
Suite de testes adversariais da Fase 0.4 passando de forma determinística, incluindo os
testes de propriedade novos.

---

## Fase 3 — Experiência de Usuário / GUI

**Objetivo**: GUI utilizável no dia a dia, competitiva com o cliente oficial de outras plataformas.

- [ ] Revisar/expandir a GUI egui existente: onboarding (login), lista de pares de pasta,
      status de sync por pasta, log de atividade em tempo real.
- [ ] Ícone de status na bandeja com estados visuais claros (sincronizado / sincronizando /
      pausado / erro) — inspirar no padrão Nextcloud/Dropbox que o ecossistema já mapeou
      (verde/azul/cinza/vermelho).
- [ ] Fluxo de "escolher pastas para sincronizar" (seletor de exclusão) com boa usabilidade.
- [ ] Tela de configurações: canal de update, estratégia de conflito, política de deleção,
      intervalo de rescan.
- [ ] Notificações desktop nativas (via `notify-rust` ou similar) para conflitos e erros.
- [ ] Ícone/identidade visual própria (não usar assets da Proton — checar trademark).

### Critério de saída
Um usuário novo consegue instalar via AUR, logar, escolher uma pasta, e ver sync funcionando
sem tocar em terminal.

---

## Fase 4 — Integração com o Desktop (o "efeito Google Drive/OneDrive")

**Objetivo**: aproximar a experiência do usuário do que ele espera de um cliente de nuvem nativo,
dentro do modelo de pasta sincronizada (sem FUSE ainda).

- [ ] Extensão Nautilus de emblem/badge de status por arquivo/pasta (o projeto `le5emeaxe/protondrive-sync`
      encontrado na pesquisa já fez algo assim em Python — pode servir de referência de
      abordagem, não de código a herdar diretamente, dado o mesmo princípio de código não
      confiável da Fase 0).
- [ ] Entrada de "Proton Drive" na sidebar do Nautilus/Dolphin apontando para a pasta sincronizada
      (via `.config/gtk-3.0/bookmarks` ou XDG equivalente — sem depender de FUSE).
      **Nota de arquitetura**: como decidido no princípio orientador, isto não é um mount virtual;
      é atalho para a pasta local sincronizada.
- [ ] Menu de contexto "Compartilhar via Proton Drive" (gera link via CLI oficial).
- [ ] Ícone de aplicação e integração com `.desktop` file / autostart via systemd `--user`.
- [ ] Integração systemd já existente do upstream (watch service) revisada e mantida.

### Critério de saída
Pasta sincronizada visível na sidebar do gerenciador de arquivos, com badges de status
por item, e ação de compartilhamento funcional pelo menu de contexto.

---

## Fase 5 — Empacotamento e Distribuição no Arch

**Objetivo**: publicação real no AUR com qualidade de pacote da comunidade.

- [ ] `PKGBUILD` final seguindo as diretrizes do Arch Wiki (namcap limpo, dependências
      corretas, `_pkgver`/`pkgrel` corretos).
- [ ] Pacote `-git` (rolling) e pacote estável versionado, como o padrão observado em
      `proton-drive-cli`/`proton-drive-cli-git` no AUR.
- [ ] `.SRCINFO` gerado e validado.
- [ ] Testar instalação limpa em container Arch (`archlinux:base` no Docker) do zero.
- [ ] Documentação de instalação e primeiros passos no README do fork.
- [ ] Submissão ao AUR com descrição clara de "unofficial", sem uso de marca Proton no nome
      do pacote de forma que sugira afiliação oficial.

### Critério de saída
Pacote instalável via `yay -S <nome-do-fork>` (ou `paru`) em uma máquina limpa, com
`namcap` sem warnings críticos.

---

## Fase 6 (avançada, opcional) — Mount FUSE Nativo sobre o CLI Oficial

**Objetivo**: entregar a experiência de "pasta montada sob demanda" sem depender de rclone,
mantendo 100% de fidelidade ao princípio de "só libs oficiais".

> Só iniciar esta fase depois que as Fases 0–5 estiverem estáveis em uso real. É o item de
> maior complexidade e maior risco do roadmap.

- [ ] Prova de conceito com a crate `fuser` (Rust, FUSE em userspace) implementando um
      filesystem read-only fino que traduz `readdir`/`open`/`read` em chamadas ao
      `proton-drive filesystem list` / `download` do CLI oficial, com cache local de metadados
      (evitar o problema de listagem lenta documentado por outros projetos do ecossistema).
- [ ] Estratégia de cache de metadados local (árvore virtual, similar à abordagem do
      `StollD/proton-drive` em Go, mas implementada com o CLI oficial como fonte de verdade,
      não com chamadas diretas à API).
- [ ] Extensão para write (`create`/`write`/`unlink`) mapeando para `upload`/delete do CLI —
      cuidado especial aqui, pois é a superfície de maior risco de perda de dados do projeto
      inteiro.
- [ ] Avaliação de performance real (latência de listagem, throughput de download) comparada
      ao modelo de pasta sincronizada da Fase 4 — decidir se o mount FUSE vira o modo padrão
      ou um modo opcional avançado.
- [ ] Nova rodada de auditoria de segurança (equivalente à Fase 0) focada especificamente no
      código FUSE novo, antes de tornar essa feature padrão.

### Critério de saída
Mount FUSE funcional, read-write, com suite de testes equivalente em rigor à da Fase 2,
e auditoria de segurança dedicada aprovada.

---

## Backlog contínuo (não bloqueia nenhuma fase)

- Suporte a múltiplas contas Proton.
- Sincronização seletiva por regra (extensão de arquivo, tamanho máximo).
- Internacionalização da GUI.
- Telemetria opt-in de erros (com consentimento explícito, nunca por padrão).
- Submissão a Flathub como formato de distribuição adicional além do AUR.

---

## Notas de uso deste documento no Claude Code

- Trabalhe **uma fase por vez**; não pule a Fase 0 mesmo que pareça "trabalho não relacionado
  a código" — ela determina que partes do fork sobrevivem intactas.
- Cada checkbox marcado deve corresponder a um commit ou PR rastreável, não a uma decisão só
  verbal.
- Ao final de cada fase, gere um resumo curto do que foi encontrado/alterado antes de avançar,
  para manter rastreabilidade de por que certas decisões foram tomadas.
