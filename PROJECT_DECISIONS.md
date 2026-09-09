# protondrive4linux — decisões do projeto

> Documento de referência pessoal (base para um post futuro, não é a postagem em si). Enumera as
> decisões tomadas, o que mudou em relação ao projeto original, e o objetivo de longo prazo.

## Contexto de origem

O projeto nasceu como um fork de [`WilhelmZA/protondrive_linux_sync`](https://github.com/WilhelmZA/protondrive_linux_sync)
("NeutronSync"): um repositório pessoal, recém-criado, sem auditoria de terceiros, sem
contribuidores externos e sem uso comprovado em produção — mas o único candidato encontrado com
um motor de sync explícito (baseline three-way-merge), GUI nativa (egui) já esboçada, e nenhuma
dependência de rclone ou SDKs não oficiais.

## Decisões tomadas

1. **Tratar o código herdado como não confiável até prova em contrário.**
   Antes de qualquer feature nova, o código foi auditado linha por linha: proveniência dos
   commits, toda dependência third-party, todo ponto de execução de processo externo, todo local
   que escreve credenciais/estado em disco, o modelo de sync completo, e o fluxo de auto-update.
   Resultado documentado em `SECURITY_AUDIT.md`.

2. **Nunca reimplementar autenticação ou criptografia por conta própria.**
   Todo acesso ao Proton Drive passa pelo `proton-drive` CLI oficial da Proton. Essa decisão
   também descartou, de saída, qualquer proposta de "cofre criptografado" feito do zero pelo
   fork — se um recurso desse tipo existir um dia, tem que delegar a cifra a uma ferramenta já
   auditada, nunca a um algoritmo próprio.

3. **Rebrand completo, não um find-and-replace.**
   NeutronSync virou **protondrive4linux**: novo nome, binários, caminho de config, variável de
   ambiente, unidades systemd, ícone e paleta de cores — decisão para ter identidade própria e
   evitar confusão de marca tanto com o projeto original quanto com a própria Proton.

4. **Adiar deliberadamente o modelo de "pasta montada" (FUSE/rclone).**
   O produto de curto/médio prazo é uma pasta local sincronizada (modelo Dropbox clássico), não
   um filesystem virtual sob demanda. Mount FUSE fica reservado para uma fase avançada e
   opcional, só depois que o resto do projeto estiver estável em uso real.

5. **Definir uma ordem de prioridade explícita para toda decisão de escopo:**
   integridade dos dados → segurança da supply chain → compatibilidade Linux → performance →
   novas funcionalidades. Nenhuma funcionalidade nova pode comprometer uma camada anterior — essa
   regra é o critério de desempate para tudo que entra ou não no roadmap.

6. **Desligar o auto-update em vez de deixá-lo rodando sem assinatura independente.**
   O único ponto do projeto com potencial de escalação de privilégio tinha checksum, mas não uma
   assinatura verificada por um canal independente do próprio artefato. Em vez de aceitar esse
   risco, a atualização automática foi desativada e a atualização passou a ser só via gerenciador
   de pacotes, até o mecanismo poder ser reescrito e auditado com calma.

7. **Escolher Arch Linux / AUR como alvo primário de distribuição**, em vez de tentar cobrir
   várias distros ao mesmo tempo desde o início.

8. **Proteger a branch `master` e exigir pull request para toda mudança**, inclusive as feitas
   com ajuda de IA — nenhum commit direto, nem para quem administra o repositório. Decisão de
   governança pensando em contribuições externas futuras.

9. **Trocar o controle do roadmap de texto solto ("Fases") por uma estrutura de Milestones/
   Issues/Labels do GitHub** (M0 a M9), com uma regra fixa: todo item marcado como concluído
   precisa apontar para um commit ou PR real — nunca para uma alegação verbal.

10. **Deprioritizar explicitamente dois recursos tentadores, em vez de deixá-los diluir o foco**:
    um "Vault" pessoal estilo OneDrive (bloqueio por senha/MFA) e um pacote maior de "Cloud Drive
    Features" (sincronização sob demanda, busca, histórico de versão, compartilhamento). Ambos
    documentados com o motivo exato de cada um estar fora da sequência principal agora — não
    descartados, só não priorizados.

11. **Manter o binário de linha de comando enxuto**: nenhuma dependência de terceiros nova no
    núcleo além do que já existe, e tudo que é exclusivo da GUI fica atrás de uma feature flag —
    quem só quer sincronizar em um servidor não carrega peso de interface gráfica.

## O que mudou em relação ao projeto original

- **Identidade**: nome, binários, ícone, paleta, caminhos de configuração e unidades systemd —
  tudo próprio, não herdado.
- **Postura de segurança**: de "sem auditoria de terceiros" para uma auditoria documentada com
  achados críticos/altos corrigidos (permissões de arquivo restritas, auto-update apontando pro
  repositório certo, cobertura de teste fechada para um cenário de perda de dados que não era
  testado).
- **Integração contínua**: de nada para `clippy` com warnings como erro, `cargo fmt --check` e
  auditoria de dependências (`cargo audit`) em todo pull request, mais um pipeline de release que
  gera `.deb`/`.rpm`/tarball e (assim que a conta da AUR estiver liberada) publica sozinho.
- **Correções reais herdadas do upstream**: um bug que fazia todo arquivo parecer "modificado" e
  disparar re-upload em massa após uma mudança de formato da CLI oficial, uma corrida na
  aquisição de lock entre duas instâncias do sync, e falhas de usabilidade na GUI (nomes de par
  duplicados faziam a lista inteira sumir, diálogos agiam no par errado).
- **Documentação**: de um `ROADMAP.md` único e solto para uma estrutura (`docs/architecture`,
  `docs/contributing`, `docs/roadmap`) onde cada afirmação de "está pronto" foi verificada de
  novo contra o código real, não copiada do documento anterior.
- **Governança pronta para colaboração**: Issues habilitadas, Milestones e labels criadas,
  dezenas de itens de trabalho rastreados publicamente — o projeto deixou de ser só uma lista de
  tarefas pessoal.

## Objetivo de longo prazo

Um cliente Linux nativo, seguro e completo para o Proton Drive, com experiência de uso no dia a
dia comparável à dos principais clientes de nuvem (Google Drive, OneDrive, Dropbox) — sem nunca
abrir mão da integridade dos dados do usuário nem da segurança da cadeia de distribuição para
chegar lá mais rápido.
