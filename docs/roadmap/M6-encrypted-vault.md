# M6 — Encrypted Vault

> **Não é uma Milestone ativa no GitHub.** Fica documentada aqui, deprioritizada, até alguém
> decidir deliberadamente puxá-la de volta — ver `docs/roadmap/ROADMAP.md` § Melhorias.

## O que é

Não um cofre com criptografia própria do fork, e sim uma pasta especial dentro do Proton Drive
que fica **bloqueada por padrão** e só é acessível depois de reautenticação (senha e/ou MFA),
com auto-lock após inatividade — o modelo exato do **OneDrive Personal Vault**. A criptografia
de conteúdo continua 100% delegada ao Proton (que já criptografa ponta-a-ponta); o fork só
adicionaria uma **camada de controle de acesso** local (gate de sessão), não uma camada de cifra
nova.

## Por que não é prioridade agora

- Depende de M1 (Data Integrity) e M2 (Supply Chain Security) estarem fechadas primeiro — um
  gate de acesso não vale nada se o motor de sync por baixo ainda pode perder ou corromper dado,
  ou se o binário distribuído não é verificável.
- Tem uma pergunta de design sem resposta, pré-requisito antes de virar issue com critério de
  aceite: este projeto sincroniza para uma pasta local real (não é FUSE/hidratação sob demanda —
  ver `M8-cloud-features.md`). Se a pasta do Vault mantém uma cópia local em texto claro o tempo
  todo, o "bloqueio" é só cosmético — qualquer processo com acesso ao disco lê o arquivo direto,
  sem nunca passar pelo gate. Duas saídas possíveis, nenhuma trivial:
  - **(a)** o Vault só materializa arquivo localmente enquanto estiver desbloqueado (reintroduz
    a discussão de hidratação sob demanda que o projeto evita), ou
  - **(b)** a cópia local do Vault fica em um container criptografado no disco (ex.:
    `gocryptfs` ou `fscrypt`, ferramentas já auditadas — **nunca** cifra escrita à mão pelo fork,
    isso violaria o princípio "não reimplementar cripto").
- Não é alta prioridade no princípio de priorização do projeto (item 5 de 5: "novas
  funcionalidades" — ver `docs/architecture/ARCHITECTURE_PRINCIPLES.md`).

## Quando revisitar

Depois que M1 e M2 fecharem, abrir como uma issue `type:research` / `status:needs-design` só
para a pergunta (a) vs (b) acima — não pular direto para implementação. Se algum dia isso virar
uma Milestone real, ela reocupa este número (M6) — não precisa renumerar as outras.

## Referência: o design original (M6-001 a M6-006, não implementadas)

Preservado para não perder o pensamento, caso a decisão de design acima seja resolvida no
futuro. **Nenhum destes itens deve ser implementado como está** — em especial M6-002
("Cryptographic design" do zero) contradiz diretamente o princípio "não reimplementar cripto":

- **Threat model**: atacante local, malware no usuário, comprometimento do Proton/GitHub, roubo
  de disco/credenciais, perda de senha, corrupção do vault.
- **Cryptographic design**: teria que usar primitivas de bibliotecas reconhecidas, nunca
  algoritmo implementado manualmente — o que, na prática, empurra para a opção (b) acima
  (delegar a um `gocryptfs`/`fscrypt` já auditado) em vez de um design de criptografia próprio.
- **Vault format specification**: versionamento, metadata, integrity, recovery, forward
  compatibility — só relevante se (b) for a rota escolhida e mesmo assim o formato seria do
  container de terceiros, não um formato próprio do fork.
- **Vault engine**: `Sync Engine → Storage Provider → {Normal Storage, Encrypted Storage}`.
- **Vault recovery**: senha correta/errada, arquivo corrompido, backup, reinstalação, computador
  novo, vault grande.
- **Vault GUI**: fluxo `Create Vault → Password → Confirm → Vault created`; operações `Unlock`,
  `Lock`, `Change password`, `Export recovery information`.
