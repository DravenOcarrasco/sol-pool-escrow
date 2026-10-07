# pool_escrow

Contrato do SOL Pool, um jogo de sinuca 8-ball no navegador onde se aposta SOL.

O programa não sabe nada de sinuca. Não conhece bolas, tacadas nem regras. Ele
guarda o dinheiro e entrega o pote, e é a única parte do sistema que precisa
ser sem confiança — a simulação roda no servidor, fora daqui. O que liga o
resultado à realidade é o replay: qualquer um reproduz a partida e confere
quem ganhou.

Está na devnet em
[`6t3nrfUE8PfYZsiKAk6S2ufJK2JhgWKGQEBN6nUL8XXc`](https://explorer.solana.com/address/6t3nrfUE8PfYZsiKAk6S2ufJK2JhgWKGQEBN6nUL8XXc?cluster=devnet).
Na mainnet, ainda não.

## O que tem aqui

**Partida 1v1.** `create_match` abre a sala com o depósito de quem criou,
`join_match` entra com o segundo depósito, `cancel_match` desiste enquanto
ninguém entrou. `settle_match` é o referee declarando o vencedor.
`publish_replay` grava o replay e **não pede assinatura do referee** — se
pedisse, devolveria ao operador o poder que essa instrução existe para tirar
dele.

**Torneio.** A inscrição se paga uma vez, em `join_tournament`, e a conta do
torneio é o próprio cofre. As partidas lá dentro são amistosas: não existe
conta nem liquidação por jogo. A melhor de 3 é isso — uma série sem depósito
por partida, em que o servidor fecha o campeão em duas vitórias.
`settle_tournament` paga o campeão pelo mesmo rateio das mesas,
`cancel_tournament` cancela e `claim_refund` é o inscrito sacando a própria
inscrição.

**Cofre de rank.** `distribute_rank_vault` paga os cinco melhores da janela,
com o `week_id` conferido on-chain contra o último distribuído — repetir
semana não passa. Quem assina é uma chave separada da do referee
(`set_rank_authority`).

**Cofres do protocolo.** `set_splits` muda a divisão do pote sem redeploy, com
duas amarras: as quatro fatias somam 10.000 bps e a do vencedor não desce
abaixo de `MIN_WINNER_BPS`. Furar esse piso exige binário novo.
`withdraw_house` custeia a operação. `burn_treasury` queima o cofre de
protocolo no incinerador, e qualquer um pode chamar: a queima não beneficia
quem aciona, e assim ela não depende de a gente estar por aqui.

**Resto.** `initialize`, `set_paused`, `set_config`, `set_authority`,
`migrate_config` e `publish_provenance`, que ancora o que é preciso para
reimplementar a física sem o nosso código — impressão digital do
comportamento, hash da especificação e onde ela está arquivada.

## O que segura o operador

`claim_timeout` é o mais importante: se o referee parar de responder, passado o
prazo **qualquer pessoa** devolve o dinheiro aos dois jogadores. Não há como
prender o dinheiro de ninguém esperando que a gente volte.

Depois dele, o piso do vencedor em `set_splits`, o `publish_replay` sem
assinatura e o `burn_treasury` aberto. São as quatro coisas que valem conferir
no código antes de confiar no jogo.

## Build

```sh
anchor build   # anchor-lang 0.31.1
```

## Fora daqui

Só o contrato está neste repositório. Física, servidor, cliente e os goldens de
replay ficam no repo principal, que é fechado — os `docs/` citados nos
comentários apontam para lá.

---

**EN** — On-chain escrow for SOL Pool, a browser 8-ball game with SOL stakes:
1v1 matches, tournaments (best-of-3 included), rank vault and protocol vaults.
The player never signs the payout; the referee settles. What holds against the
operator: `claim_timeout` lets anyone refund both players if the referee stops
responding, `set_splits` cannot push the winner's share below `MIN_WINNER_BPS`,
`publish_replay` needs no referee signature, and `burn_treasury` is callable by
anyone. Simulation is off-chain, auditable through on-chain replay hashes.
Devnet only for now.
