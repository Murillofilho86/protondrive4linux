# M7 — Distribution

## M7-001 — Flatpak
**Status: 🔒 Bloqueado indefinidamente — decisão de M3-005: não viável hoje.** A pesquisa de
viabilidade (`M3-linux-compatibility.md#m3-005`) concluiu que tanto `GUI only` quanto
`GUI + sync daemon` dependem de alcançar o `proton-drive` CLI de dentro do sandbox, e isso só é
possível via sandbox escape total (`flatpak-spawn --host`, `--talk-name=org.freedesktop.Flatpak`)
ou embutindo um binário proprietário de terceiro sem necessidade reduzida de permissões — nenhuma
das duas é uma distribuição Flathub aceitável. Bloqueado até a Proton distribuir o CLI de forma
compatível com sandbox (Flatpak/AppImage oficial próprio, ou MIT sem depender do runtime `bun`).
Não implementar enquanto essa condição não mudar.

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
**Status: ✅ Concluído** (`src/doctor.rs`). O `doctor` deixou de ser só uma ferramenta de debug do
adapter e virou o diagnóstico de instalação: uma linha por check, com a correção embaixo quando algo
pede atenção. **Só lê** — nunca cria nem conserta nada — então é seguro rodar a qualquer momento.

### Como funciona
- **A lib devolve dados, a CLI imprime.** `doctor::run(...) -> Report`; tudo que um check olha fora
  do processo passa por `Env` (caminhos, uid) ou pelo trait `Host` (systemd, rede), então os testes
  cobrem cada desfecho sem conta Proton, sem systemd e sem rede. Uma GUI pode exibir o mesmo
  `Report` depois sem tocar na lógica.
- **Credentials sem ler a sessão.** O princípio do projeto (`SECURITY_AUDIT.md`) é nunca tocar em
  token/senha: a sessão fica no keyring, gerida pelo CLI oficial. Então "presença, não conteúdo" é
  perguntar ao CLI do mesmo jeito que o watcher e a GUI perguntam (listar a raiz remota e ler
  "faça login" da falha — `protoncli::is_not_logged_in`), de modo que os três nunca discordem.
  Um keyring inalcançável (`failed to load session`: trancado, sem D-Bus, headless) é distinguido de
  "nunca logou", porque logar de novo não resolve o primeiro. Um erro que não é de login (rede)
  vira "não deu pra verificar", não "deslogado".
- **Permissões = o contrato que o app já cumpre** (`0700` pastas, `0600` arquivos — `config::write_private`,
  `logger.rs`, `stats.rs`). Dono diferente (criado com `sudo`) e escrita por outros são falha; leitura
  por outros é aviso. Config de propriedade do root (provisionado pelo administrador, `-c /etc/...`)
  é só uma nota — root já pode tudo e o config só é lido —, mas a pasta de *estado* do root é falha:
  o app precisa escrever nela. O config manda no `cli.binary`, então quem escreve nele (ou na pasta dele) roda
  código como o usuário. Pasta com sticky bit (ex. `/tmp`) não conta como gravável.
- **Rede = TCP/443 para `drive-api.proton.me`**, sem enviar nada, com DNS num thread auxiliar para
  não travar quando o resolver está fora. Atrás de proxy a falha vira aviso (o teste é direto e o CLI
  pode estar usando o proxy); só os *nomes* das variáveis de proxy aparecem, nunca os valores.
- **Sem config, o resto continua**: CLI, rede, systemd e launcher não dependem dele (CLI roda com o
  nome padrão). `-c` apontando para um arquivo inexistente é erro aqui (os outros comandos caem em
  silêncio para o próximo local de busca).
- **Timeouts curtos** (30 s no CLI em vez dos 1800 s de transferência): um diagnóstico não pode
  travar. Credentials é pulado se a rede falhou (não adianta esperar o CLI estourar o timeout).
- Saída: `--json` (para scripts e relatos de bug), `--offline` (pula rede e login), `--probe`/
  `REMOTE_PATH` (o dump antigo do adapter, agora opcional). Exit 0 a menos que algo falhe (avisos não
  contam).

### Achados no caminho (corrigidos aqui)
- `init` criava o config com permissões padrão (0644 numa pasta 0755), ao contrário de todo outro
  caminho que escreve o config; o `doctor` novo reclamaria de uma instalação recém-criada. `init`
  agora usa `config::write_private` (`0700`/`0600`) e `init --force` aperta um arquivo antigo.
- O probe antigo (`raw_list`) engolia o erro do CLI e imprimia "(empty)", indistinguível de uma
  pasta vazia, e não passava `--` antes do caminho como o resto do código.
- M2-005 dizia que `doctor` mostrava a versão com commit; só o `status` mostrava. Agora mostra.

### Critérios de aceite
- [x] Proton CLI (resolução + versão) — e aviso se for mais antigo que o 0.8.0 verificado.
- [x] Credentials (presença, não conteúdo).
- [x] Filesystem (permissões de `state_dir`/config — `0700`/`0600`).
- [x] systemd (unit ativa? — incluindo serviço e timer ativos ao mesmo tempo).
- [x] Desktop entry presente (e coerente com o binário da GUI).
- [x] Network (conectividade básica).
- [x] Version (do próprio binário, não só do CLI — M2-005).

### Fora do escopo (possíveis próximos passos)
- Limite do inotify (`fs.inotify.max_user_watches`) contra o tamanho das árvores dos pares.
- Uma página de diagnóstico na GUI exibindo o mesmo `Report`.
- `doctor --fix`: de propósito não existe — o diagnóstico é só leitura; as correções vêm como
  comandos para o usuário rodar.

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
