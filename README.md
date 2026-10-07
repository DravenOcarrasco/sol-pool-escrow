# SOL Pool — escrow de partidas e torneios (pool_escrow)

O contrato on-chain do SOL Pool: um jogo de sinuca 8-ball no navegador onde se
aposta SOL nativo.

Este programa é **a única parte do sistema que precisa ser sem confiança**. Ele
não sabe nada sobre sinuca — não conhece bolas, tacadas nem regras. Guarda o
dinheiro das pessoas e o entrega segundo regras que ninguém, nem o operador do
jogo, consegue burlar depois de o programa estar no ar.

A simulação roda fora daqui, no servidor. O que amarra o resultado à realidade
é o **replay**: qualquer um reproduz a partida e confere o vencedor.

## Instruções

### Partida 1v1

| instrução | o que faz |
|---|---|
| `create_match` | A deposita; a sala abre |
| `join_match` | B deposita; a partida está comprometida |
| `cancel_match` | A desiste antes de B entrar e recebe de volta |
| `settle_match` | o referee declara o vencedor; o pote é distribuído |
| `claim_timeout` | o referee sumiu; **qualquer um** devolve o dinheiro aos dois |
| `publish_replay` | grava o replay. **Não exige assinatura do referee** — exigir devolveria ao operador o poder que a instrução existe para tirar dele |

### Torneio (inclui a melhor de 3)

| instrução | o que faz |
|---|---|
| `create_tournament` | abre o torneio; a conta dele é o próprio cofre |
| `join_tournament` | a inscrição é paga **uma vez**, no registro |
| `settle_tournament` | o campeão leva o pote, pelo mesmo rateio das mesas; assina o referee |
| `cancel_tournament` | cancela o torneio |
| `claim_refund` | o inscrito saca a própria inscrição de um torneio cancelado |

As partidas dentro de um torneio seguem amistosas: não existe conta por partida
nem liquidação por partida — o dinheiro é do torneio. A **melhor de 3** é isso:
uma série sem depósito por jogo, em que o servidor decide o campeão por duas
vitórias.

### Cofre de rank

| instrução | o que faz |
|---|---|
| `init_rank_vault` | cria o cofre |
| `distribute_rank_vault` | distribui aos 5 melhores da janela; o `week_id` é conferido on-chain contra `last_week_id` |
| `set_rank_authority` | define quem assina a distribuição (chave separada da do referee) |

### Dinheiro do protocolo

| instrução | o que faz |
|---|---|
| `init_vaults` | cria os cofres da casa e do protocolo |
| `set_splits` | ajusta a divisão do pote sem redeploy. As quatro fatias somam exatamente 10.000 bps, e **o vencedor nunca pode cair abaixo de `MIN_WINNER_BPS`** |
| `withdraw_house` | retira do cofre da casa para custear a operação |
| `burn_treasury` | queima o cofre de protocolo mandando ao incinerador. **Qualquer um pode chamar** — a queima não beneficia quem aciona, e deixar aberto evita que a promessa dependa de a operação estar viva |

### Administração

`initialize`, `set_paused`, `set_config`, `set_authority`, `migrate_config`,
`publish_provenance`.

`publish_provenance` ancora on-chain o que é preciso para reimplementar a
simulação **sem o nosso código**: a impressão digital do comportamento da
física, o hash do documento de especificação e onde ele está arquivado.

## As garantias contra o operador

Vale ler esta lista com desconfiança — ela é o ponto do programa:

1. `claim_timeout`: se o referee parar de responder, **qualquer pessoa**
   devolve o dinheiro aos dois jogadores. O operador não prende os fundos.
2. `MIN_WINNER_BPS`: existe um piso para a fatia do vencedor que `set_splits`
   não consegue furar. Baixá-lo exige um binário novo e visível.
3. `publish_replay` sem assinatura do referee: a prova do resultado não depende
   de o operador querer publicá-la.
4. `burn_treasury` aberta a qualquer um: a queima prometida não depende de nós.

## Onde está no ar

- **devnet**: [`6t3nrfUE8PfYZsiKAk6S2ufJK2JhgWKGQEBN6nUL8XXc`](https://explorer.solana.com/address/6t3nrfUE8PfYZsiKAk6S2ufJK2JhgWKGQEBN6nUL8XXc?cluster=devnet)
- mainnet: ainda não.

## Compilar

```sh
anchor build          # anchor-lang 0.31.1
```

## O que não está neste repositório

Só o contrato mora aqui. O motor de física, o servidor, o cliente e os goldens
de replay ficam no repositório principal, que é privado. As referências a
`docs/` nos comentários do código apontam para lá.

## In English

On-chain escrow for the SOL Pool 8-ball game: 1v1 matches and tournaments
(including best-of-3 series) staked in native SOL. The program holds the stakes
and releases the pot on settlement by a referee key; the player never signs the
payout.

Guarantees that hold against the operator: `claim_timeout` lets **anyone**
refund both players if the referee goes away; `MIN_WINNER_BPS` is a floor on the
winner's share that `set_splits` cannot breach; `publish_replay` needs no
referee signature, so the proof of the result does not depend on us publishing
it; and `burn_treasury` is callable by anyone. Game simulation happens off-chain
and is auditable through on-chain replay hashes, with `publish_provenance`
anchoring what is needed to reimplement the physics without our code.

Live on devnet at the address above; not on mainnet yet.
