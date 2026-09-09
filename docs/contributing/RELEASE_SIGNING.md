# Release Signing (M2-001)

Como assinamos os artefatos de release (`.deb`, `.rpm`, tarballs) e como isso é mantido.
Ver `docs/roadmap/M2-supply-chain.md` para o porquê disso importar.

## Modelo escolhido: GPG com chave mestra + subchave de assinatura

Não usamos a chave mestra pra assinar releases direto. O fluxo é:

- **Chave mestra** (capacidade só de *Certify*): gerada uma vez, guardada com cuidado,
  **nunca** entra em CI, nunca sai da sua máquina/mídia offline. É a sua identidade de longo
  prazo pro projeto.
- **Subchave de assinatura** (capacidade só de *Sign*, com expiração, ex: 1 ano): é essa que
  vai pro GitHub Actions como secret e assina cada release.

Por quê separar assim: se a subchave vazar ou expirar, você **revoga só ela** e emite outra —
sua identidade (a chave mestra, e a confiança que quem verifica já depositou nela) continua
válida. Isso também é o que permite, no futuro, dar a um segundo mantenedor de confiança uma
subchave própria pra cortar release sem nunca expor a chave mestra. É o mesmo modelo usado por
Debian/Ubuntu para assinatura de repositório.

## Gerando a chave (uma vez, feito por você)

Rode isso na sua máquina, de preferência num ambiente que você confia bastante — a chave mestra
que sai daqui é o segredo mais sensível do projeto.

```sh
gpg --expert --full-generate-key
```

- Tipo de chave: `(11) ECC (set your own capabilities)`.
- Capacidades: desmarque tudo exceto **Certify** (`Toggle` até sobrar só `Certify`).
- Curva: `Curve 25519`.
- Expiração: `0` (chave mestra não expira — fica offline, então expiração não protege muito e
  só cria trabalho de renovação).
- Nome/e-mail: use algo que identifique o projeto, ex. `protondrive4linux release signing
  <security@seu-dominio-ou-github>` — não precisa ser seu e-mail pessoal.

Anote o **fingerprint** que aparece no final (`gpg --list-secret-keys --keyid-format=long`
mostra de novo se precisar).

### Adicionar a subchave de assinatura

```sh
gpg --expert --edit-key <FINGERPRINT-DA-MESTRA>
```

Dentro do prompt interativo do `gpg`:

```
gpg> addkey
```
- Tipo: `(10) ECC (sign only)`.
- Curva: `Curve 25519`.
- Expiração: `1y` (renovável — ver seção "Renovação" abaixo).
- Confirme, coloque a senha da chave mestra quando pedir.
```
gpg> save
```

### Certificado de revogação

Gere e guarde **fora do repositório**, em algum lugar seguro (não no disco onde a chave mestra
vive junto, se puder — o objetivo é conseguir revogar mesmo se perder a chave mestra):

```sh
gpg --output protondrive4linux-revoke.asc --gen-revoke <FINGERPRINT-DA-MESTRA>
```

### Exportar a chave pública (essa vai pro repositório)

```sh
gpg --armor --export <FINGERPRINT-DA-MESTRA> > docs/keys/protondrive4linux-release.asc
```

### Exportar **só a subchave secreta** (essa vai pro GitHub Actions)

O `!` no final do ID é importante — sem ele o `gpg` tentaria exportar a chave mestra secreta
junto.

```sh
gpg --list-secret-keys --keyid-format=long <FINGERPRINT-DA-MESTRA>
# pega o keyid da linha "ssb" (a subchave de assinatura), não da "sec"

gpg --armor --export-secret-subkeys <KEYID-DA-SUBCHAVE>! > /tmp/protondrive4linux-signing-subkey.asc
```

## Configurando o GitHub Actions

Três valores, dois secrets (sensíveis) e uma variável (não-sensível, é só o fingerprint):

```sh
gh secret set GPG_SIGNING_KEY < /tmp/protondrive4linux-signing-subkey.asc
gh secret set GPG_SIGNING_KEY_PASSPHRASE   # cole a senha quando pedir; não passe como argumento
gh variable set GPG_SIGNING_KEY_FINGERPRINT --body "<FINGERPRINT-DA-SUBCHAVE-COMPLETO>"
```

Depois, **apague o arquivo temporário da subchave**:

```sh
shred -u /tmp/protondrive4linux-signing-subkey.asc   # ou: rm -P se shred não estiver disponível
```

Confirme que o secret existe (`gh secret list` não mostra o valor, só o nome — o que é o
esperado):

```sh
gh secret list
```

## Ativando a assinatura em CI

O workflow (`.github/workflows/release.yml`) só assina se a variável de repositório
`RELEASE_SIGNING_READY` estiver `true` — antes disso, o step é pulado de propósito (senão
nenhum release poderia sair antes da chave existir). Depois de configurar os três valores acima:

```sh
gh variable set RELEASE_SIGNING_READY --body true
```

A partir daí, todo tag `v*` novo:
1. Falha a build se `GPG_SIGNING_KEY`/`GPG_SIGNING_KEY_FINGERPRINT` não existirem (não faz
   release não-assinado silenciosamente uma vez que isso está "ligado").
2. Assina cada artefato (`.deb`, `.rpm`, tarballs) com `.asc` destacado.
3. Falha a build se algum artefato ficar sem assinatura correspondente.

Commite `docs/keys/protondrive4linux-release.asc` no mesmo PR/branch onde ligar isso, pra
`SECURITY.md` e o `README` terem pra onde apontar.

## Renovação da subchave (antes de expirar)

```sh
gpg --edit-key <FINGERPRINT-DA-MESTRA>
gpg> key 1        # seleciona a subchave de assinatura (a "ssb")
gpg> expire
# nova data, ex: 1y
gpg> save

gpg --armor --export <FINGERPRINT-DA-MESTRA> > docs/keys/protondrive4linux-release.asc
gpg --armor --export-secret-subkeys <KEYID-DA-SUBCHAVE>! > /tmp/protondrive4linux-signing-subkey.asc
gh secret set GPG_SIGNING_KEY < /tmp/protondrive4linux-signing-subkey.asc
shred -u /tmp/protondrive4linux-signing-subkey.asc
```

Commite a chave pública atualizada (a data de expiração faz parte da assinatura sobre a
subchave, então o arquivo público muda mesmo a subchave em si continuando a mesma).

## Delegando pra um segundo mantenedor (futuro)

Quando fizer sentido: gere uma subchave de assinatura **separada** com `addkey` a partir da
mesma chave mestra, exporte só ela (`export-secret-subkeys <keyid-da-nova-subchave>!`), entregue
por um canal seguro. Se essa pessoa sair do projeto ou a chave dela vazar, revogue só a subchave
dela (`gpg --edit-key`, `key N`, `revkey`) — a identidade do projeto (a chave mestra) e a
confiança que os usuários já depositaram nela continuam intactas.
