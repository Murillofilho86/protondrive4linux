# Definition of Done

Applies to every issue tracked under `docs/roadmap/`. A milestone is not done because the code
works — see the per-milestone files (`docs/roadmap/M*.md`) for milestone-level exit criteria on
top of this.

## Regra de progressão

Uma milestone não pode ser considerada concluída apenas porque o código funciona. Cada milestone
precisa cumprir:

```text
Implementation + Automated Tests + Failure Tests + Documentation + Review
```

## Definition of Done global (por issue)

- [ ] Código implementado.
- [ ] Testes automatizados.
- [ ] Testes de regressão.
- [ ] Documentação atualizada.
- [ ] Logs adequados.
- [ ] Tratamento de erro implementado.
- [ ] Nenhuma regressão conhecida.
- [ ] CI passando.
- [ ] Code review concluído.

### Para `risk:data-loss`

- [ ] Teste de falha.
- [ ] Teste de recuperação.
- [ ] Teste de interrupção.

### Para `risk:security`

- [ ] Threat model atualizado.
- [ ] Security test.
- [ ] Dependency audit.
- [ ] Documentation.

## Regra de ouro

Cada checkbox marcado num arquivo `docs/roadmap/M*.md` deve corresponder a um commit ou PR
rastreável, nunca a uma decisão só verbal. Se não dá pra apontar pro commit/PR, o item não está
concluído — está "parece que sim".
