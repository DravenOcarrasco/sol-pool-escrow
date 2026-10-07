//! Escrow de partidas 1v1 de 8-Ball, em SOL nativo.
//!
//! Este programa é a ÚNICA parte do sistema que precisa ser sem confiança.
//! Ele não sabe nada sobre sinuca: não conhece bolas, tacadas nem regras. Ele
//! guarda o dinheiro de duas pessoas e o entrega segundo regras que ninguém —
//! nem o operador do jogo — consegue burlar depois que o programa está no ar.
//!
//! A simulação da partida roda fora daqui, no servidor (ver docs/TDD.md §4).
//! O que amarra o resultado à realidade é o REPLAY, gravado on-chain: qualquer
//! um reproduz a partida e confere o vencedor.
//!
//! Fluxo:
//!   create_match  → A deposita, sala aberta
//!   join_match    → B deposita, partida comprometida
//!   cancel_match  → A desiste antes de B entrar, recebe de volta
//!   settle_match  → referee declara o vencedor, pote é distribuído
//!   claim_timeout → referee sumiu; qualquer um devolve o dinheiro aos dois

use anchor_lang::prelude::*;
use anchor_lang::solana_program::hash::hash;
use anchor_lang::system_program;

// ATENÇÃO: para o deploy do cofre de rank, este precisa ser um Program ID
// NOVO, gerado do zero — não o endereço já em uso no devnet. A equipe decidiu
// deployar sob uma identidade nova de propósito, para não perturbar quem já
// está testando o programa atual (ver docs/SETUP-SOLANA.md, seção "Deploy do
// cofre de rank"). Ao trocar, atualize `PROGRAM_ID` em
// `packages/chain-client/src/index.ts` NO MESMO commit.
declare_id!("6t3nrfUE8PfYZsiKAk6S2ufJK2JhgWKGQEBN6nUL8XXc");

pub const BPS_DENOMINATOR: u64 = 10_000;

/// Divisão inicial do pote: 90% vencedor, 5% casa, 5% protocolo.
///
/// São só o PONTO DE PARTIDA — os valores vivem no `Config` e a autoridade
/// ajusta com `set_splits`, sem recompilar. O rake certo é uma questão
/// empírica: 10% é alto para jogo de habilidade, e descobrir o número bom
/// exige medir retenção, não redeployar a cada palpite.
pub const DEFAULT_WINNER_BPS: u16 = 9_000;
pub const DEFAULT_HOUSE_BPS: u16 = 500;
pub const DEFAULT_PROTOCOL_BPS: u16 = 500;

/**
 * Teto de bytes do replay gravado on-chain.
 *
 * Casado com `MAX_REPLAY_BYTES` do pacote `@sol-pool/replay`. Os tetos de lá
 * foram dimensionados por MEDIÇÃO de partidas simuladas, não por chute.
 *
 * O teto não vem daqui, vem da transação: ela não passa de 1232 bytes, e um
 * `settle_match` sem replay nenhum já gasta 510 com assinaturas, contas e
 * discriminador. Sobram 721, e 682 deixa margem para uma instrução de compute
 * budget.
 *
 * Este número JÁ ESTEVE ERRADO DUAS VEZES: dizia 856, de quando o cabeçalho
 * tinha 56 bytes, e a conta partia de "~900 bytes de dados por transação", que
 * é otimista demais. O efeito seria a liquidação falhar só nas partidas mais
 * longas, com dinheiro na mesa e nenhum teste curto acusando. Há um teste no
 * pacote `replay` que trava a igualdade entre os dois lados; se ele falhar,
 * corrija AQUI também.
 */
pub const MAX_REPLAY_BYTES: usize = 656;

/// Tamanho máximo do endereço de arquivamento da especificação.
/// Uma URL do Arweave tem ~62 caracteres; 128 dá folga para IPFS e afins.
pub const MAX_SPEC_URI_LEN: usize = 128;

/// Piso do que o vencedor recebe. É a proteção que NÃO depende de confiança:
/// nem a autoridade consegue transformar o jogo num rake de 50%. Mudar este
/// limite exige publicar um binário novo, o que é público e auditável.
pub const MIN_WINNER_BPS: u16 = 8_500;

/**
 * Prazo de uma partida COMPROMETIDA, decidido pelo contrato.
 *
 * O `timeout_seconds` do `create_match` governa só a sala esperando oponente —
 * é escolha do criador e só afeta o dinheiro dele. Depois que o segundo
 * jogador deposita, o relógio é REANCORADO aqui.
 *
 * A separação existe porque a versão anterior deixava o prazo da partida nas
 * mãos do criador, e isso era um free-roll: bastava assinar `create_match` com
 * 60 segundos em vez dos 3600 que o servidor sugere. A partida vencia antes da
 * segunda tacada, e quem estava perdendo acionava `claim_timeout` e recebia a
 * entrada de volta. Toda derrota virava empate, com o risco todo do outro lado.
 *
 * Foi reproduzido em devnet antes desta correção.
 */
pub const COMMITTED_TIMEOUT_SECONDS: i64 = 3_600;

/// Endereço incinerador da Solana. Lamports enviados para cá são destruídos
/// pelo runtime no fim do slot — some do supply, verificável no explorer.
///
/// É o análogo de `spl_token::burn` para SOL nativo. Quando o jogo passar a
/// aceitar um token SPL, `burn_treasury` troca esta transferência por um burn
/// de verdade; o resto da estrutura não muda.
pub const INCINERATOR: Pubkey = anchor_lang::solana_program::incinerator::ID;

#[program]
pub mod pool_escrow {
    use super::*;

    /// Configura o programa. Só roda uma vez.
    pub fn initialize(
        ctx: Context<Initialize>,
        referee: Pubkey,
        min_stake: u64,
        max_stake: u64,
    ) -> Result<()> {
        require!(min_stake > 0 && max_stake >= min_stake, EscrowError::InvalidStakeRange);

        let config = &mut ctx.accounts.config;
        config.authority = ctx.accounts.authority.key();
        config.referee = referee;
        config.min_stake = min_stake;
        config.max_stake = max_stake;
        config.paused = false;
        config.bump = ctx.bumps.config;
        config.winner_bps = DEFAULT_WINNER_BPS;
        config.house_bps = DEFAULT_HOUSE_BPS;
        config.protocol_bps = DEFAULT_PROTOCOL_BPS;
        Ok(())
    }

    /// Cria a mesa e deposita a entrada do criador.
    ///
    /// O depósito acontece AQUI, antes da mesa existir para qualquer efeito.
    /// Não há estado intermediário em que uma mesa esteja aberta sem lastro.
    pub fn create_match(
        ctx: Context<CreateMatch>,
        match_id: [u8; 16],
        stake: u64,
        timeout_seconds: i64,
        commit: [u8; 32],
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, EscrowError::Paused);
        require!(
            stake >= config.min_stake && stake <= config.max_stake,
            EscrowError::StakeOutOfRange
        );
        require!(
            (60..=86_400).contains(&timeout_seconds),
            EscrowError::InvalidTimeout
        );

        /*
         * O pote precisa cobrir o registro permanente — CONFERIDO AQUI, na
         * entrada, e não só na liquidação.
         *
         * `settle_match` já recusava um pote pequeno demais com
         * `PotTooSmallForRecord`. Mas recusar lá é tarde: o dinheiro dos dois
         * jogadores já está preso, a partida já foi jogada, e a única saída
         * vira o reembolso pelo prazo. O vencedor recebe a entrada de volta em
         * vez do pote, por um limite que dava para conferir antes de aceitar o
         * primeiro lamport.
         *
         * A trava não é hipotética: `min_stake` é ajustável pela autoridade, e
         * nada a impedia de descer abaixo do aluguel. Hoje o mínimo é 0,01 SOL
         * contra 0,00231 de aluguel — mas a segurança não deve depender de
         * ninguém lembrar dessa conta ao mexer na configuração.
         */
        let aluguel = Rent::get()?.minimum_balance(MatchRecordV3::LEN);
        require!(
            stake.checked_mul(2).ok_or(EscrowError::MathOverflow)? > aluguel,
            EscrowError::PotTooSmallForRecord
        );

        let now = Clock::get()?.unix_timestamp;
        let game = &mut ctx.accounts.game;
        game.match_id = match_id;
        game.creator = ctx.accounts.creator.key();
        game.opponent = Pubkey::default();
        game.stake = stake;
        game.state = MatchState::Waiting as u8;
        game.created_at = now;
        game.deadline = now
            .checked_add(timeout_seconds)
            .ok_or(EscrowError::MathOverflow)?;
        game.commit_creator = commit;
        game.bump = ctx.bumps.game;

        deposit(
            &ctx.accounts.creator,
            &ctx.accounts.game.to_account_info(),
            &ctx.accounts.system_program,
            stake,
        )
    }

    /// Segundo jogador entra e deposita o mesmo valor.
    pub fn join_match(ctx: Context<JoinMatch>, commit: [u8; 32]) -> Result<()> {
        require!(!ctx.accounts.config.paused, EscrowError::Paused);

        let now = Clock::get()?.unix_timestamp;
        let stake = {
            let game = &ctx.accounts.game;
            require!(game.state == MatchState::Waiting as u8, EscrowError::NotJoinable);
            require!(
                game.creator != ctx.accounts.opponent.key(),
                EscrowError::CannotJoinOwnMatch
            );
            require!(now < game.deadline, EscrowError::MatchExpired);
            game.stake
        };

        deposit(
            &ctx.accounts.opponent,
            &ctx.accounts.game.to_account_info(),
            &ctx.accounts.system_program,
            stake,
        )?;

        let game = &mut ctx.accounts.game;
        game.opponent = ctx.accounts.opponent.key();
        game.commit_opponent = commit;
        game.state = MatchState::Committed as u8;

        // O relógio da PARTIDA começa agora, com duração que o contrato define.
        // Antes disto o prazo continuava sendo o que o criador escolheu para a
        // sala, e ele podia escolher 60 segundos.
        game.deadline = now
            .checked_add(COMMITTED_TIMEOUT_SECONDS)
            .ok_or(EscrowError::MathOverflow)?;

        emit!(MatchCommitted {
            match_id: game.match_id,
            deadline: game.deadline,
        });

        Ok(())
    }

    /// Cancela uma mesa que nunca recebeu oponente. O depósito volta ao criador.
    ///
    /// Depois que a partida está comprometida, este caminho fecha: sair de uma
    /// partida em andamento é derrota, não reembolso.
    ///
    /// Antes do prazo, só o criador pode cancelar. **Depois do prazo, qualquer
    /// um pode** — e o dinheiro volta para o criador de qualquer forma, porque
    /// o destino é fixado por `close = creator`. Sem isso, um criador que
    /// sumisse deixaria o próprio SOL preso na PDA para sempre.
    pub fn cancel_match(ctx: Context<CancelMatch>) -> Result<()> {
        let game = &ctx.accounts.game;
        require!(game.state == MatchState::Waiting as u8, EscrowError::NotCancellable);

        if ctx.accounts.signer.key() != game.creator {
            let now = Clock::get()?.unix_timestamp;
            require!(now >= game.deadline, EscrowError::NotCreator);
        }
        // A conta é fechada por `close = creator`, o que devolve stake + rent.
        Ok(())
    }

    /// Liquida a partida, distribuindo o pote conforme a divisão do Config.
    ///
    /// Assinado pelo referee, cuja chave está no Config. O que vai para a chain
    /// é o HASH do replay: quem tiver os bytes confere contra ele e reproduz a
    /// partida por conta própria.
    pub fn settle_match(
        ctx: Context<SettleMatch>,
        winner: Pubkey,
        replay_hash: [u8; 32],
        replay_len: u16,
        nonce_creator: [u8; 32],
        nonce_opponent: [u8; 32],
    ) -> Result<()> {
        // O teto continua valendo, agora como sanidade: um comprimento absurdo
        // seria sinal de replay que nenhum verificador nosso sabe ler.
        require!(
            (replay_len as usize) <= MAX_REPLAY_BYTES && replay_len > 0,
            EscrowError::ReplayTooLarge
        );
        let game = &ctx.accounts.game;
        require!(game.state == MatchState::Committed as u8, EscrowError::NotSettleable);
        require!(
            winner == game.creator || winner == game.opponent,
            EscrowError::WinnerNotInMatch
        );
        require!(
            ctx.accounts.winner.key() == winner,
            EscrowError::WinnerAccountMismatch
        );

        // Os nonces têm de bater com os compromissos feitos ao depositar. É o
        // que amarra o seed do replay a uma escolha que os DOIS jogadores
        // fizeram antes de saber o resultado — sem isto o referee escolhia o
        // seed e fabricava a partida inteira.
        require!(
            hash(&nonce_creator).to_bytes() == game.commit_creator,
            EscrowError::BadReveal
        );
        require!(
            hash(&nonce_opponent).to_bytes() == game.commit_opponent,
            EscrowError::BadReveal
        );

        let pot_bruto = game
            .stake
            .checked_mul(2)
            .ok_or(EscrowError::MathOverflow)?;

        /*
         * A PERMANÊNCIA É PAGA PELO POTE, não pela casa.
         *
         * O registro fica na blockchain para sempre, e isso exige um depósito
         * de isenção de aluguel. NÃO é uma cobrança recorrente — os lamports
         * ficam parados na conta e voltariam se ela fosse fechada. Mas nunca
         * fechamos o registro, porque ele é a prova da partida: na prática o
         * capital fica imobilizado para sempre.
         *
         * Quem imobilizava era o referee — ou seja, nós. A conta estava
         * invertida: numa mesa de 0.01 SOL o depósito era 0.00685 e o rake da
         * casa é 0.001. Cada partida pequena travava quase sete vezes a
         * receita dela, sem limite de acumulação.
         *
         * Descontar do pote resolveu o lado errado da conta. Guardar só o hash
         * resolveu o tamanho dela: de ~0,0057 para 0,00231, fixo, porque o
         * registro deixou de crescer com a partida.
         */
        let aluguel = Rent::get()?.minimum_balance(MatchRecordV3::LEN);
        let pot = pot_bruto
            .checked_sub(aluguel)
            .ok_or(EscrowError::PotTooSmallForRecord)?;

        let (winner_bps, house_bps, _, rank_bps) = ctx.accounts.config.splits4();
        let (prize, treasury_cut, house_cut, rank_cut) = split_pot(pot, winner_bps, house_bps, rank_bps)?;

        // A conta da partida é do programa, então movemos lamports direto.
        // O que sobrar (o rent) volta ao criador quando a conta é fechada.
        let game_info = ctx.accounts.game.to_account_info();
        let treasury_info = ctx.accounts.treasury_vault.to_account_info();
        let house_info = ctx.accounts.house_vault.to_account_info();

        // Devolve ao referee o aluguel que ele adiantou ao criar o registro.
        move_lamports(&game_info, &ctx.accounts.referee.to_account_info(), aluguel)?;

        move_lamports(&game_info, &ctx.accounts.winner, prize)?;
        move_lamports(&game_info, &treasury_info, treasury_cut)?;
        move_lamports(&game_info, &house_info, house_cut)?;
        let rank_info = ctx.accounts.rank_vault.to_account_info();
        move_lamports(&game_info, &rank_info, rank_cut)?;

        let match_id = game.match_id;

        // Contabilidade acumulada: sem isto só dá para somar as taxas lendo o
        // histórico inteiro de transações, o que fica caro e frágil.
        let treasury = &mut ctx.accounts.treasury_vault;
        treasury.total_in = treasury
            .total_in
            .checked_add(treasury_cut)
            .ok_or(EscrowError::MathOverflow)?;

        let house = &mut ctx.accounts.house_vault;
        house.total_in = house.total_in.checked_add(house_cut).ok_or(EscrowError::MathOverflow)?;
        house.matches_settled = house.matches_settled.saturating_add(1);

        let rank_vault = &mut ctx.accounts.rank_vault;
        rank_vault.total_in = rank_vault
            .total_in
            .checked_add(rank_cut)
            .ok_or(EscrowError::MathOverflow)?;

        // Grava o registro PERMANENTE da partida.
        //
        // Evento em log seria mais barato, mas logs são podados pelos nós com
        // o tempo. Uma conta persiste enquanto pagar aluguel — e é isso que
        // sustenta a promessa de auditoria: daqui a anos, com o site fora do
        // ar, os bytes continuam lá para qualquer um verificar.
        let record = &mut ctx.accounts.record;
        record.match_id = match_id;
        record.winner = winner;
        record.loser = if winner == game.creator { game.opponent } else { game.creator };
        record.creator_won = winner == game.creator;
        record.nonce_creator = nonce_creator;
        record.nonce_opponent = nonce_opponent;
        record.pot = pot;
        record.settled_at = Clock::get()?.unix_timestamp;
        record.replay_hash = replay_hash;
        record.replay_len = replay_len;
        record.bump = ctx.bumps.record;

        emit!(MatchSettled {
            match_id,
            winner,
            pot,
            prize,
            treasury: treasury_cut,
            house: house_cut,
            rank: rank_cut,
            result_hash: replay_hash,
        });

        Ok(())
    }

    /**
     * Publica os BYTES do replay na blockchain.
     *
     * O registro guarda o hash, não os bytes, e isso resolve INTEGRIDADE:
     * ninguém adultera um replay sem que o hash denuncie. O que não resolve é
     * DISPONIBILIDADE. Os bytes vivem nos dois clientes e num arquivo nosso, e
     * se sumirem de lá a partida vira "não arquivada" — o estado em que ninguém
     * consegue provar fraude nem inocência. É a única saída do sistema que o
     * operador controla sozinho, e é o que esta instrução fecha.
     *
     * Os bytes não ficam em estado de conta. Ficam nos DADOS DA TRANSAÇÃO, que
     * vivem no ledger: a taxa da Solana é por assinatura e não por byte, então
     * carregá-los custa a taxa base, e não os 6.960 lamports por byte que o
     * aluguel cobraria para guardá-los em conta. A contrapartida honesta é que
     * recuperar o ledger antigo exige um nó de arquivo, o que não é a
     * permanência garantida de uma conta — mas troca "depende do nosso servidor"
     * por "depende de qualquer arquivista da rede".
     *
     * SEM DONO, de propósito, e é essa a parte que importa. Um jogador que
     * recebeu os bytes no `match.end` pode publicá-los mesmo que a gente não
     * queira. Exigir a assinatura do referee aqui devolveria ao operador
     * exatamente o poder que a instrução existe para tirar dele.
     *
     * Não precisa ser atômica com a liquidação: o hash já está gravado, e
     * qualquer publicação posterior confere contra ele. Se falhar, repete-se —
     * e quem repete pode ser outra pessoa.
     *
     * A verificação on-chain não é enfeite. Sem ela, qualquer um poderia
     * publicar lixo dizendo ser o replay da partida, e um indexador não saberia
     * qual transação tem os bytes certos. Com ela, TODA `publish_replay` que
     * confirma contém provadamente o replay real — o comprimento vem do registro
     * e o teto do `MAX_REPLAY_BYTES` já foi imposto na liquidação, então o
     * tamanho está limitado sem precisar de mais uma checagem.
     */
    pub fn publish_replay(ctx: Context<PublishReplay>, replay: Vec<u8>) -> Result<()> {
        let record = &ctx.accounts.record;

        require!(
            replay.len() == record.replay_len as usize,
            EscrowError::ReplayLenMismatch
        );
        require!(
            hash(&replay).to_bytes() == record.replay_hash,
            EscrowError::ReplayHashMismatch
        );

        emit!(ReplayPublished {
            match_id: record.match_id,
            replay_len: record.replay_len,
        });

        Ok(())
    }

    /// Prazo estourado sem liquidação. Devolve o depósito aos dois.
    ///
    /// Qualquer um pode chamar — é de propósito. Se o referee sumir ou o
    /// servidor morrer, o dinheiro não fica preso: depois do prazo, um terceiro
    /// qualquer pode destravar os fundos, e eles só voltam para os jogadores.
    pub fn claim_timeout(ctx: Context<ClaimTimeout>) -> Result<()> {
        let game = &ctx.accounts.game;
        let now = Clock::get()?.unix_timestamp;
        require!(now >= game.deadline, EscrowError::NotExpiredYet);
        require!(
            game.state == MatchState::Committed as u8,
            EscrowError::NotRefundable
        );

        let stake = game.stake;
        let game_info = ctx.accounts.game.to_account_info();
        // O criador recebe o resto (stake + rent) no fechamento da conta.
        move_lamports(&game_info, &ctx.accounts.opponent, stake)?;

        emit!(MatchRefunded {
            match_id: game.match_id,
            stake,
        });

        Ok(())
    }

    /// Pausa de emergência. Impede novas mesas; não afeta as em andamento.
    pub fn set_paused(ctx: Context<AdminOnly>, paused: bool) -> Result<()> {
        ctx.accounts.config.paused = paused;
        Ok(())
    }

    /// Troca as chaves de operação.
    ///
    /// Necessário por dois motivos práticos. Primeiro, rotação: se a chave do
    /// referee vazar, sem isto o programa inteiro vira sucata (ver §6.5 do
    /// TDD). Segundo, recuperação: uma configuração inicial errada deixaria
    /// `settle_match` permanentemente inacessível.
    ///
    /// Argumento `None` mantém o valor atual.
    pub fn set_config(
        ctx: Context<AdminOnly>,
        referee: Option<Pubkey>,
        min_stake: Option<u64>,
        max_stake: Option<u64>,
    ) -> Result<()> {
        let config = &mut ctx.accounts.config;

        if let Some(key) = referee {
            config.referee = key;
        }
        if let Some(value) = min_stake {
            config.min_stake = value;
        }
        if let Some(value) = max_stake {
            config.max_stake = value;
        }

        require!(
            config.min_stake > 0 && config.max_stake >= config.min_stake,
            EscrowError::InvalidStakeRange
        );

        // Um mínimo abaixo do aluguel do registro abriria uma faixa de apostas
        // que o contrato ACEITA e depois não consegue liquidar. Melhor a
        // autoridade descobrir aqui, ao configurar, do que dois jogadores
        // descobrirem com o dinheiro preso.
        let aluguel = Rent::get()?.minimum_balance(MatchRecordV3::LEN);
        require!(
            config
                .min_stake
                .checked_mul(2)
                .ok_or(EscrowError::MathOverflow)?
                > aluguel,
            EscrowError::PotTooSmallForRecord
        );

        // Trocar o referee é a mudança mais perigosa que a autoridade faz:
        // quem assina as liquidações passa a ser outro. Era a única sem
        // rastro, enquanto os saques e a divisão já emitiam evento.
        emit!(ConfigChanged {
            referee: config.referee,
            min_stake: config.min_stake,
            max_stake: config.max_stake,
        });
        Ok(())
    }

    /// Passa a autoridade para outra chave. Caminho só de ida — confira duas
    /// vezes antes, porque não há como desfazer sem a chave nova.
    pub fn set_authority(ctx: Context<AdminOnly>, authority: Pubkey) -> Result<()> {
        let previous = ctx.accounts.config.authority;
        ctx.accounts.config.authority = authority;

        emit!(AuthorityChanged {
            previous,
            current: authority,
        });
        Ok(())
    }

    /// Migra o Config para o layout atual.
    ///
    /// Cobre os dois formatos antigos: o original (sem divisão configurável) e
    /// o intermediário (com divisão, mas ainda carregando `house`/`treasury`
    /// como chaves soltas). Aqueles dois campos morreram quando os cofres
    /// viraram PDAs — ficavam ali confundindo quem lesse a configuração.
    ///
    /// Idempotente: rodar de novo num Config já migrado não faz nada.
    pub fn migrate_config(ctx: Context<MigrateConfig>) -> Result<()> {
        let info = ctx.accounts.config.to_account_info();
        let tamanho = info.data_len();

        if tamanho == Config::LEN {
            return Ok(()); // já migrado
        }
        require!(
            tamanho == Config::LEN_V1 || tamanho == Config::LEN_V2 || tamanho == Config::LEN_V3,
            EscrowError::NotConfigAccount
        );

        // Acesso cru de propósito: `Account<Config>` desserializaria com o
        // layout NOVO e falharia numa conta que ainda está no antigo.
        let (authority, referee, min_stake, max_stake, paused, bump, splits) = {
            let data = info.try_borrow_data()?;
            require!(data[..8] == Config::DISCRIMINATOR[..], EscrowError::NotConfigAccount);

            let ler_chave = |inicio: usize| -> Result<Pubkey> {
                Pubkey::try_from(&data[inicio..inicio + 32])
                    .map_err(|_| error!(EscrowError::NotConfigAccount))
            };
            let ler_u64 = |i: usize| u64::from_le_bytes(data[i..i + 8].try_into().unwrap());
            let ler_u16 = |i: usize| u16::from_le_bytes(data[i..i + 2].try_into().unwrap());

            // V3 tem os campos colados direto após referee, sem house/treasury.
            // V1/V2 tinham house/treasury ocupando 72..136, empurrando tudo depois.
            let (min_stake, max_stake, paused, bump, splits) = if tamanho == Config::LEN_V3 {
                (
                    ler_u64(72),
                    ler_u64(80),
                    data[88],
                    data[89],
                    (ler_u16(90), ler_u16(92), ler_u16(94)),
                )
            } else {
                // V1/V2: house/treasury ocupavam 72..136, empurrando tudo depois.
                let splits = if tamanho == Config::LEN_V2 {
                    (ler_u16(154), ler_u16(156), ler_u16(158))
                } else {
                    (DEFAULT_WINNER_BPS, DEFAULT_HOUSE_BPS, DEFAULT_PROTOCOL_BPS)
                };
                (ler_u64(136), ler_u64(144), data[152], data[153], splits)
            };

            (
                ler_chave(8)?,
                ler_chave(40)?,
                min_stake,
                max_stake,
                paused,
                bump,
                splits,
            )
        };

        require_keys_eq!(authority, ctx.accounts.authority.key(), EscrowError::NotAuthority);

        /*
         * A conta precisa ter `Config::LEN` bytes ANTES de qualquer escrita
         * que alcance além do tamanho atual dela.
         *
         * `info.try_borrow_mut_data()` devolve uma fatia do tamanho ATUAL da
         * conta (`info.data_len()`), não de `Config::LEN`. Toda migração
         * anterior a esta ENCOLHIA a conta (154/160 → 96 bytes), e escrever
         * antes de redimensionar era seguro porque as posições escritas já
         * cabiam no tamanho antigo, maior. Esta é a primeira migração que
         * CRESCE a conta (96 → 130, ao ganhar `rank_authority` e `rank_bps`)
         * — escrever `data[96..130]` antes do resize estoura a fatia de 96
         * bytes e a instrução panica.
         *
         * O resize sozinho não basta: crescer a conta aumenta o aluguel
         * mínimo de isenção, e uma conta V3 só tem rent para 96 bytes. Sem
         * cobrir a diferença, a conta fica devendo aluguel e o runtime
         * rejeita a transação. Diferente do encolhimento (que sobra
         * lamports), crescer pode faltar — e falta aqui, sempre: 96 bytes já
         * pagos rendem menos rent do que 130 bytes exigem.
         */
        let rent = Rent::get()?.minimum_balance(Config::LEN);
        let faltante = rent.saturating_sub(info.lamports());
        if faltante > 0 {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer {
                        from: ctx.accounts.authority.to_account_info(),
                        to: info.clone(),
                    },
                ),
                faltante,
            )?;
        }

        info.resize(Config::LEN)?;

        // Reescreve no layout novo. Tudo já foi lido para variáveis locais, então
        // sobrescrever posições que se sobrepõem é seguro — e agora a conta já
        // tem `Config::LEN` bytes, então escrever até `data[128..130]` é válido
        // também no caso de V3 (96 → 130), que cresce.
        {
            let mut data = info.try_borrow_mut_data()?;
            data[8..40].copy_from_slice(authority.as_ref());
            data[40..72].copy_from_slice(referee.as_ref());
            data[72..80].copy_from_slice(&min_stake.to_le_bytes());
            data[80..88].copy_from_slice(&max_stake.to_le_bytes());
            data[88] = paused;
            data[89] = bump;
            let (w, h, p) = splits;
            data[90..92].copy_from_slice(&if w == 0 { DEFAULT_WINNER_BPS } else { w }.to_le_bytes());
            data[92..94].copy_from_slice(&if w == 0 { DEFAULT_HOUSE_BPS } else { h }.to_le_bytes());
            data[94..96].copy_from_slice(&if w == 0 { DEFAULT_PROTOCOL_BPS } else { p }.to_le_bytes());
            data[96..128].copy_from_slice(Pubkey::default().as_ref());
            data[128..130].copy_from_slice(&0u16.to_le_bytes());
        }

        // Conta MENOR precisa de menos aluguel (caso V1/V2, que encolhem);
        // o excedente volta a quem pagou. No caso V3 (que cresce), a conta
        // já recebeu exatamente o que faltava acima, então `sobra` é 0 aqui.
        let sobra = info.lamports().saturating_sub(rent);
        if sobra > 0 {
            move_lamports(&info, &ctx.accounts.authority.to_account_info(), sobra)?;
        }
        Ok(())
    }

    /// Ajusta a divisão do pote sem redeployar.
    ///
    /// As quatro fatias precisam somar exatamente 10_000, e o vencedor nunca
    /// pode cair abaixo de `MIN_WINNER_BPS`. Esse piso é a garantia que o
    /// jogador tem contra o operador: baixá-lo exige um binário novo, que é
    /// público e auditável — não basta uma transação discreta.
    pub fn set_splits(
        ctx: Context<AdminOnly>,
        winner_bps: u16,
        house_bps: u16,
        protocol_bps: u16,
        rank_bps: u16,
    ) -> Result<()> {
        let soma = winner_bps as u64 + house_bps as u64 + protocol_bps as u64 + rank_bps as u64;
        require!(soma == BPS_DENOMINATOR, EscrowError::SplitsDoNotSum);
        require!(winner_bps >= MIN_WINNER_BPS, EscrowError::WinnerShareTooLow);

        let config = &mut ctx.accounts.config;
        config.winner_bps = winner_bps;
        config.house_bps = house_bps;
        config.protocol_bps = protocol_bps;
        config.rank_bps = rank_bps;

        emit!(SplitsChanged { winner_bps, house_bps, protocol_bps, rank_bps });
        Ok(())
    }

    /// Troca quem pode assinar `distribute_rank_vault`.
    ///
    /// Separada de `set_splits` de propósito: a fatia (quanto) e a
    /// autoridade (quem paga) são decisões independentes, e uma auditoria
    /// de "quem pode mexer no cofre de rank" não deveria precisar ler a
    /// divisão do pote inteira.
    pub fn set_rank_authority(ctx: Context<AdminOnly>, rank_authority: Pubkey) -> Result<()> {
        ctx.accounts.config.rank_authority = rank_authority;
        emit!(RankAuthorityChanged { rank_authority });
        Ok(())
    }

    /// Publica a procedência de uma versão da física.
    ///
    /// Ancora on-chain o que é preciso para reimplementar a simulação sem o
    /// nosso código: a impressão digital do comportamento, o hash do documento
    /// de especificação e onde ele está arquivado.
    ///
    /// IMUTÁVEL depois de criada, e isso é a garantia inteira. Se fosse
    /// editável, alguém poderia trocar a especificação depois de partidas
    /// terem sido jogadas — e replays antigos passariam a "provar" outra
    /// coisa. Especificação errada exige publicar uma VERSÃO NOVA da física,
    /// o que é público e deixa a antiga intacta.
    pub fn publish_provenance(
        ctx: Context<PublishProvenance>,
        engine_version: u16,
        physics_digest: [u8; 8],
        spec_hash: [u8; 32],
        spec_uri: String,
    ) -> Result<()> {
        require!(spec_uri.len() <= MAX_SPEC_URI_LEN, EscrowError::SpecUriTooLong);
        require!(!spec_uri.is_empty(), EscrowError::SpecUriTooLong);

        let p = &mut ctx.accounts.provenance;
        p.engine_version = engine_version;
        p.physics_digest = physics_digest;
        p.spec_hash = spec_hash;
        p.spec_uri = spec_uri;
        p.published_at = Clock::get()?.unix_timestamp;
        p.bump = ctx.bumps.provenance;

        emit!(ProvenancePublished {
            engine_version,
            physics_digest,
            spec_hash,
        });
        Ok(())
    }

    /// Cria os dois cofres. Roda uma vez.
    ///
    /// Cofres são PDAs do programa: não existe chave privada para perder nem
    /// para vazar. É a diferença entre um endereço que depende de um arquivo
    /// em disco e um que depende só do código publicado.
    pub fn init_vaults(ctx: Context<InitVaults>) -> Result<()> {
        let house = &mut ctx.accounts.house_vault;
        house.kind = VaultKind::House as u8;
        house.bump = ctx.bumps.house_vault;

        let treasury = &mut ctx.accounts.treasury_vault;
        treasury.kind = VaultKind::Treasury as u8;
        treasury.bump = ctx.bumps.treasury_vault;
        Ok(())
    }

    /// Cria o cofre de rank. Separada de `init_vaults` porque aquela já
    /// rodou contra house/treasury; rodar de novo criaria uma conta cujo
    /// endereço já existe e falharia — melhor uma instrução nova e pequena.
    pub fn init_rank_vault(ctx: Context<InitRankVault>) -> Result<()> {
        let vault = &mut ctx.accounts.rank_vault;
        vault.kind = VaultKind::Rank as u8;
        vault.bump = ctx.bumps.rank_vault;
        Ok(())
    }

    /// Retira do cofre da casa para custear a operação.
    ///
    /// Só a autoridade. O saldo mínimo de aluguel fica intocado, senão a conta
    /// seria coletada e a contabilidade histórica se perderia.
    pub fn withdraw_house(ctx: Context<WithdrawHouse>, amount: u64) -> Result<()> {
        let vault_info = ctx.accounts.house_vault.to_account_info();
        let disponivel = withdrawable(&vault_info)?;
        require!(amount > 0 && amount <= disponivel, EscrowError::InsufficientVaultBalance);

        move_lamports(&vault_info, &ctx.accounts.destination, amount)?;

        let vault = &mut ctx.accounts.house_vault;
        vault.total_out = vault.total_out.checked_add(amount).ok_or(EscrowError::MathOverflow)?;

        emit!(HouseWithdrawn {
            amount,
            destination: ctx.accounts.destination.key(),
            total_out: vault.total_out,
        });
        Ok(())
    }

    /// Queima do cofre de protocolo.
    ///
    /// Manda para o incinerador, que destrói os lamports de fato. Qualquer um
    /// pode chamar — a queima não beneficia quem aciona, e deixar aberto evita
    /// que a promessa de queima dependa de a operação estar viva.
    pub fn burn_treasury(ctx: Context<BurnTreasury>, amount: u64) -> Result<()> {
        require!(
            ctx.accounts.incinerator.key() == INCINERATOR,
            EscrowError::NotIncinerator
        );

        let vault_info = ctx.accounts.treasury_vault.to_account_info();
        let disponivel = withdrawable(&vault_info)?;
        require!(amount > 0 && amount <= disponivel, EscrowError::InsufficientVaultBalance);

        move_lamports(&vault_info, &ctx.accounts.incinerator, amount)?;

        let vault = &mut ctx.accounts.treasury_vault;
        vault.total_out = vault.total_out.checked_add(amount).ok_or(EscrowError::MathOverflow)?;

        emit!(TreasuryBurned { amount, total_burned: vault.total_out });
        Ok(())
    }

    /// Distribui o cofre de rank aos 5 melhores da janela. NÃO zera a
    /// contabilidade — `total_out` só acumula, mesmo padrão de
    /// `withdraw_house`/`burn_treasury`. `week_id` viaja no evento, para o
    /// indexador ligar esta distribuição à janela calculada no servidor, e
    /// também é CONFERIDO on-chain contra `last_week_id`: um `week_id` que
    /// não seja estritamente maior que o último pago é recusado, o que
    /// impede reenviar o mesmo pagamento (ver doc de `Vault::last_week_id`).
    pub fn distribute_rank_vault(
        ctx: Context<DistributeRankVault>,
        top5: [Pubkey; 5],
        amounts: [u64; 5],
        week_id: u32,
    ) -> Result<()> {
        // Backstop ON-CHAIN contra reenvio do mesmo pagamento — ver o doc de
        // `Vault::last_week_id`. Confere ANTES de mover qualquer lamport:
        // um restart do servidor no meio do estado "pagou mas não indexou"
        // não tem mais como repetir o pagamento, porque a trava mora aqui,
        // não na memória do processo que reiniciou.
        require!(
            week_id > ctx.accounts.rank_vault.last_week_id,
            EscrowError::RankWeekAlreadyDistributed
        );

        let total: u64 = amounts.iter().try_fold(0u64, |acc, &a| {
            acc.checked_add(a).ok_or(EscrowError::MathOverflow)
        })?;
        let vault_info = ctx.accounts.rank_vault.to_account_info();
        let disponivel = withdrawable(&vault_info)?;
        require!(total <= disponivel, EscrowError::RankAmountsExceedVault);

        let recipients: [&AccountInfo; 5] = [
            &ctx.accounts.recipient0,
            &ctx.accounts.recipient1,
            &ctx.accounts.recipient2,
            &ctx.accounts.recipient3,
            &ctx.accounts.recipient4,
        ];
        for (recipient, &amount) in recipients.iter().zip(amounts.iter()) {
            if amount > 0 {
                move_lamports(&vault_info, recipient, amount)?;
            }
        }

        let vault = &mut ctx.accounts.rank_vault;
        vault.total_out = vault.total_out.checked_add(total).ok_or(EscrowError::MathOverflow)?;
        vault.last_week_id = week_id;

        emit!(RankVaultDistributed { week_id, top5, amounts, total });
        Ok(())
    }

    // ------------------------------------------------------------- torneio

    /*
     * TORNEIO COM POTE: a inscricao e paga UMA vez, no registro, e o campeao
     * leva o acumulado pelo mesmo rateio das mesas.
     *
     * Por que instrucao nova, e nao um cofre do servidor: pagar o campeao
     * exige que alguem fixe o DESTINO do dinheiro. O varredor
     * (`apps/server/src/sweeper.ts`) documenta a doutrina do projeto -- quem
     * aciona NAO escolhe o destino --, e um cofre guardado pelo servidor a
     * violaria de frente. Aqui os `address =` e os `close =` fixam os
     * destinos, como no resto do programa.
     *
     * As PARTIDAS do torneio seguem amistosas: nao existe conta `Game` por
     * partida nem liquidacao por partida. O dinheiro e do torneio, e a conta
     * do torneio e o proprio cofre -- o mesmo arranjo do `Game`, que guarda
     * os depositos nele mesmo.
     */

    /// Abre o torneio. A conta dele e o cofre.
    pub fn create_tournament(
        ctx: Context<CreateTournament>,
        tournament_id: [u8; 16],
        seats: u8,
        stake: u64,
        timeout_seconds: i64,
    ) -> Result<()> {
        let config = &ctx.accounts.config;
        require!(!config.paused, EscrowError::Paused);
        require!(
            stake >= config.min_stake && stake <= config.max_stake,
            EscrowError::StakeOutOfRange
        );
        /*
         * Potencia de 2, as chaves que o servidor sabe montar -- e 2, que nao
         * e chave nenhuma: e a melhor de 3.
         *
         * Duas vagas foi o que faltava para o evento `melhor-de-3` ter
         * deposito. O servidor ja decide o campeao da serie (duas vitorias) e
         * o cofre daqui nao sabe o que e uma chave: guarda o pote, conta as
         * inscricoes e paga um vencedor. Com `seats = 2` a serie paga nasce
         * sem instrucao nova, sem conta nova e sem segundo cofre para manter.
         *
         * A diferenca que fica na TELA, nao aqui: no torneio quem cria
         * organiza e nao joga; na serie quem cria e um dos dois, e se inscreve
         * no mesmo ato.
         */
        require!(matches!(seats, 2 | 4 | 8 | 16), EscrowError::BadSeats);
        // Ate uma semana: torneio com etapa marcada pode durar dias, o que a
        // mesa 1x1 (teto de um dia) nunca precisou.
        require!(
            (60..=604_800).contains(&timeout_seconds),
            EscrowError::InvalidTimeout
        );

        /*
         * O pote tem de cobrir o aluguel das contas permanentes -- conferido
         * AQUI, na abertura, e nao na liquidacao.
         *
         * E a mesma razao do `PotTooSmallForRecord` do `create_match`:
         * recusar na liquidacao e tarde, porque o dinheiro de todos ja esta
         * preso e a unica saida vira a devolucao. Aqui e pior que na mesa,
         * porque sao ate 16 contas de inscricao, e `min_stake` e ajustavel
         * pela autoridade -- nada a impede de descer abaixo do aluguel.
         */
        let rent = Rent::get()?;
        let aluguel = rent
            .minimum_balance(Tournament::LEN)
            .checked_add(
                rent.minimum_balance(TournamentEntry::LEN)
                    .checked_mul(seats as u64)
                    .ok_or(EscrowError::MathOverflow)?,
            )
            .ok_or(EscrowError::MathOverflow)?;
        require!(
            stake
                .checked_mul(seats as u64)
                .ok_or(EscrowError::MathOverflow)?
                > aluguel,
            EscrowError::PotTooSmallForRecord
        );

        let now = Clock::get()?.unix_timestamp;
        let t = &mut ctx.accounts.tournament;
        t.tournament_id = tournament_id;
        t.creator = ctx.accounts.creator.key();
        t.stake = stake;
        t.seats = seats;
        t.joined = 0;
        t.pot = 0;
        t.state = TournamentState::Open as u8;
        t.deadline = now
            .checked_add(timeout_seconds)
            .ok_or(EscrowError::MathOverflow)?;
        t.bump = ctx.bumps.tournament;

        emit!(TournamentCreated {
            tournament_id,
            creator: t.creator,
            stake,
            seats,
        });
        Ok(())
    }

    /// O jogador se registra e deposita a inscricao. Uma vez, nao por partida.
    pub fn join_tournament(ctx: Context<JoinTournament>) -> Result<()> {
        require!(!ctx.accounts.config.paused, EscrowError::Paused);
        let now = Clock::get()?.unix_timestamp;

        let stake = {
            let t = &ctx.accounts.tournament;
            require!(
                t.state == TournamentState::Open as u8,
                EscrowError::NotJoinable
            );
            require!(t.joined < t.seats, EscrowError::TournamentFull);
            require!(now <= t.deadline, EscrowError::DeadlinePassed);
            t.stake
        };

        /*
         * A `TournamentEntry` e PDA de (torneio, jogador), e `init` recusa a
         * segunda chamada do mesmo jogador: e a garantia de uma inscricao por
         * pessoa, dada pelo runtime e nao por uma busca em vetor.
         *
         * E e ela que o saque individual usa para marcar quem ja recebeu --
         * um vetor de 16 chaves dentro do `Tournament` engordaria a conta e
         * nao daria nenhuma das duas coisas.
         */
        let entry = &mut ctx.accounts.entry;
        entry.tournament = ctx.accounts.tournament.key();
        entry.player = ctx.accounts.player.key();
        entry.bump = ctx.bumps.entry;

        deposit(
            &ctx.accounts.player,
            &ctx.accounts.tournament.to_account_info(),
            &ctx.accounts.system_program,
            stake,
        )?;

        let t = &mut ctx.accounts.tournament;
        t.joined = t.joined.checked_add(1).ok_or(EscrowError::MathOverflow)?;
        t.pot = t.pot.checked_add(stake).ok_or(EscrowError::MathOverflow)?;
        // Cheio: comeca a jogar, e ninguem mais entra nem cancela por prazo.
        if t.joined == t.seats {
            t.state = TournamentState::Playing as u8;
        }

        emit!(TournamentJoined {
            tournament_id: t.tournament_id,
            player: entry.player,
            joined: t.joined,
            pot: t.pot,
        });
        Ok(())
    }

    /// O campeao leva o pote, pelo MESMO rateio das mesas. Assina o referee.
    pub fn settle_tournament(ctx: Context<SettleTournament>, champion: Pubkey) -> Result<()> {
        let pot = {
            let t = &ctx.accounts.tournament;
            require!(
                t.state == TournamentState::Playing as u8,
                EscrowError::NotSettleable
            );
            require_keys_eq!(
                ctx.accounts.champion.key(),
                champion,
                EscrowError::WinnerAccountMismatch
            );
            t.pot
        };

        /*
         * Zero regra de rateio nova: e a tabela do `Config`, a mesma da mesa.
         * O campeao do torneio e o vencedor de uma mesa cujo pote e a soma das
         * inscricoes -- e por isso que esta instrucao e curta.
         */
        let (winner_bps, house_bps, _, rank_bps) = ctx.accounts.config.splits4();
        let (prize, treasury_cut, house_cut, rank_cut) =
            split_pot(pot, winner_bps, house_bps, rank_bps)?;

        let t_info = ctx.accounts.tournament.to_account_info();
        let treasury_info = ctx.accounts.treasury_vault.to_account_info();
        let house_info = ctx.accounts.house_vault.to_account_info();
        let rank_info = ctx.accounts.rank_vault.to_account_info();

        /*
         * O aluguel das `TournamentEntry` NAO esta neste cofre, e tentar
         * devolve-lo daqui quebrava a liquidacao inteira.
         *
         * Era o que esta instrucao fazia, por uma premissa errada minha: cada
         * `TournamentEntry` e uma conta PROPRIA, criada com
         * `init, payer = player`, entao o aluguel dela foi direto do jogador
         * para a conta dela -- nunca passou pelo cofre do torneio. Tirar esse
         * valor daqui dava `InsufficientEscrowBalance`, e NENHUM torneio pago
         * era liquidado. So o teste contra a chain pegou: a guarda de fonte
         * que eu tinha escrito EXIGIA o comportamento errado.
         *
         * O aluguel de cada inscricao continua na conta dela: na devolucao o
         * `close = player` do `claim_refund` o devolve ao jogador; no caminho
         * da liquidacao ele fica preso ali -- divida conhecida, registrada em
         * `docs/ONDE-PARAMOS.md`.
         */
        move_lamports(&t_info, &ctx.accounts.champion, prize)?;
        move_lamports(&t_info, &treasury_info, treasury_cut)?;
        move_lamports(&t_info, &house_info, house_cut)?;
        move_lamports(&t_info, &rank_info, rank_cut)?;

        let house = &mut ctx.accounts.house_vault;
        house.total_in = house
            .total_in
            .checked_add(house_cut)
            .ok_or(EscrowError::MathOverflow)?;

        let t = &mut ctx.accounts.tournament;
        t.state = TournamentState::Settled as u8;

        emit!(TournamentSettled {
            tournament_id: t.tournament_id,
            champion,
            pot,
            prize,
        });
        Ok(())
    }

    /**
     * Cancela o torneio: a partir daqui cada inscrito saca o deposito dele.
     *
     * O referee cancela quando quiser (torneio que nao encheu, sala desfeita);
     * QUALQUER UM cancela depois do prazo. E a mesma rede de seguranca do
     * `claim_timeout`: o dinheiro nao pode ficar preso porque o servidor caiu.
     *
     * Seguro de deixar aberto porque quem aciona NAO escolhe destino nenhum --
     * isto so muda o estado, e o destino de cada lamport esta no `claim_refund`,
     * preso ao `address = entry.player`.
     */
    pub fn cancel_tournament(ctx: Context<CancelTournament>) -> Result<()> {
        let now = Clock::get()?.unix_timestamp;
        let referee = ctx.accounts.config.referee;
        let t = &mut ctx.accounts.tournament;
        require!(
            t.state == TournamentState::Open as u8 || t.state == TournamentState::Playing as u8,
            EscrowError::NotCancellable
        );
        require!(
            ctx.accounts.caller.key() == referee || now > t.deadline,
            EscrowError::NotExpiredYet
        );

        t.state = TournamentState::Cancelled as u8;
        emit!(TournamentCancelled {
            tournament_id: t.tournament_id,
            pot: t.pot,
        });
        Ok(())
    }

    /// O inscrito saca a propria inscricao de um torneio cancelado.
    pub fn claim_refund(ctx: Context<ClaimRefund>) -> Result<()> {
        let stake = {
            let t = &ctx.accounts.tournament;
            require!(
                t.state == TournamentState::Cancelled as u8,
                EscrowError::NotRefundable
            );
            t.stake
        };

        /*
         * O segundo saque e impossivel pelo `close = player` da `entry`: a
         * conta deixa de existir, e sem ela a PDA nao resolve. A mesma ideia
         * do `close` no `claim_timeout`.
         */
        move_lamports(
            &ctx.accounts.tournament.to_account_info(),
            &ctx.accounts.player.to_account_info(),
            stake,
        )?;

        let t = &mut ctx.accounts.tournament;
        t.pot = t.pot.saturating_sub(stake);
        t.joined = t.joined.saturating_sub(1);

        emit!(TournamentRefunded {
            tournament_id: t.tournament_id,
            player: ctx.accounts.player.key(),
            amount: stake,
        });
        Ok(())
    }
}

// ---------------------------------------------------------------- helpers

/// Divide o pote em 4. O arredondamento sobra para o tesouro, nunca para o
/// vencedor nem para o rank — assim a soma nunca excede o pote.
fn split_pot(
    pot: u64,
    winner_bps: u16,
    house_bps: u16,
    rank_bps: u16,
) -> Result<(u64, u64, u64, u64)> {
    let prize = (pot as u128 * winner_bps as u128 / BPS_DENOMINATOR as u128) as u64;
    let house = (pot as u128 * house_bps as u128 / BPS_DENOMINATOR as u128) as u64;
    let rank = (pot as u128 * rank_bps as u128 / BPS_DENOMINATOR as u128) as u64;
    let treasury = pot
        .checked_sub(prize)
        .and_then(|r| r.checked_sub(house))
        .and_then(|r| r.checked_sub(rank))
        .ok_or(EscrowError::MathOverflow)?;
    Ok((prize, treasury, house, rank))
}

/// Deposita do jogador para a conta da partida, via System Program.
fn deposit<'info>(
    from: &Signer<'info>,
    to: &AccountInfo<'info>,
    system_program: &Program<'info, System>,
    amount: u64,
) -> Result<()> {
    system_program::transfer(
        CpiContext::new(
            system_program.to_account_info(),
            system_program::Transfer {
                from: from.to_account_info(),
                to: to.clone(),
            },
        ),
        amount,
    )
}

/// Quanto dá para tirar de um cofre sem torná-lo coletável por falta de aluguel.
fn withdrawable(vault: &AccountInfo) -> Result<u64> {
    let rent = Rent::get()?.minimum_balance(vault.data_len());
    Ok(vault.lamports().saturating_sub(rent))
}

/// Move lamports de uma conta do programa para outra conta.
///
/// Não usa CPI porque a conta de origem pertence a este programa — mexer nos
/// lamports diretamente é o caminho correto e evita a restrição do System
/// Program de não transferir de contas com dados.
fn move_lamports<'info>(from: &AccountInfo<'info>, to: &AccountInfo<'info>, amount: u64) -> Result<()> {
    if amount == 0 {
        return Ok(());
    }
    let mut from_lamports = from.try_borrow_mut_lamports()?;
    let mut to_lamports = to.try_borrow_mut_lamports()?;

    **from_lamports = from_lamports
        .checked_sub(amount)
        .ok_or(EscrowError::InsufficientEscrowBalance)?;
    **to_lamports = to_lamports
        .checked_add(amount)
        .ok_or(EscrowError::MathOverflow)?;
    Ok(())
}

// ---------------------------------------------------------------- contas

#[account]
pub struct Config {
    pub authority: Pubkey,
    /// Chave que declara o vencedor. Ver docs/TDD.md §6.4 sobre a confiança
    /// que isso exige e o que a contrabalança.
    pub referee: Pubkey,
    pub min_stake: u64,
    pub max_stake: u64,
    pub paused: bool,
    pub bump: u8,
    /// Divisão do pote em basis points. Somam 10_000.
    pub winner_bps: u16,
    pub house_bps: u16,
    pub protocol_bps: u16,
    /// Autoridade que assina `distribute_rank_vault`. Separada do `referee`
    /// de propósito: vazar uma chave não compromete a outra função.
    pub rank_authority: Pubkey,
    /// Fatia do cofre de rank. Zero até a autoridade configurar via
    /// `set_splits`; enquanto for zero, `rank_authority` também é ignorada.
    pub rank_bps: u16,
}

impl Config {
    /// Layout original: tinha `house` e `treasury` como chaves soltas.
    pub const LEN_V1: usize = 8 + 32 * 4 + 8 * 2 + 1 + 1;
    /// V1 + os três campos de divisão do pote.
    pub const LEN_V2: usize = Self::LEN_V1 + 2 * 3;
    /// V2 sem `house`/`treasury` soltos: os cofres viraram PDAs.
    pub const LEN_V3: usize = 8 + 32 * 2 + 8 * 2 + 1 + 1 + 2 * 3;
    /// Atual: soma a autoridade e a fatia do cofre de rank.
    pub const LEN: usize = Self::LEN_V3 + 32 + 2;

    /// Divisão de 4 fatias. `rank_bps` zero é um Config que ainda não
    /// configurou o cofre de rank — não é erro, é o estado inicial.
    pub fn splits4(&self) -> (u16, u16, u16, u16) {
        if self.winner_bps == 0 {
            (DEFAULT_WINNER_BPS, DEFAULT_HOUSE_BPS, DEFAULT_PROTOCOL_BPS, 0)
        } else {
            (self.winner_bps, self.house_bps, self.protocol_bps, self.rank_bps)
        }
    }
}

#[account]
pub struct Game {
    pub match_id: [u8; 16],
    pub creator: Pubkey,
    pub opponent: Pubkey,
    pub stake: u64,
    pub state: u8,
    pub created_at: i64,
    pub deadline: i64,
    /**
     * Compromissos com os nonces que definem a quebra.
     *
     * Cada jogador escolhe 32 bytes em segredo e publica só o hash — AQUI, ao
     * depositar, antes de saber o do adversário. O seed da partida é
     * `sha256(nonce_criador ‖ nonce_oponente)`, e `settle_match` só aceita
     * nonces que batam com estes compromissos.
     *
     * Sem isso, nada on-chain amarrava o seed do replay a coisa nenhuma: o
     * referee escolhia o seed que quisesse e fabricava uma partida inteira que
     * nunca aconteceu, com o vencedor que quisesse. Foi demonstrado em duas
     * tentativas de busca aleatória.
     */
    pub commit_creator: [u8; 32],
    pub commit_opponent: [u8; 32],
    pub bump: u8,
}

impl Game {
    pub const LEN: usize = 8 + 16 + 32 * 2 + 8 + 1 + 8 * 2 + 32 * 2 + 1;
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MatchState {
    Waiting = 0,
    Committed = 1,
}

// ---------------------------------------------------------------- contextos

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(
        init,
        payer = authority,
        space = Config::LEN,
        seeds = [b"config"],
        bump
    )]
    pub config: Account<'info, Config>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(match_id: [u8; 16])]
pub struct CreateMatch<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = creator,
        space = Game::LEN,
        seeds = [b"match", match_id.as_ref()],
        bump
    )]
    pub game: Account<'info, Game>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct JoinMatch<'info> {
    #[account(mut)]
    pub opponent: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [b"match", game.match_id.as_ref()],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct CancelMatch<'info> {
    /// Antes do prazo precisa ser o criador; depois, qualquer um. A checagem
    /// está na instrução porque depende do relógio.
    pub signer: Signer<'info>,
    #[account(
        mut,
        close = creator,
        seeds = [b"match", game.match_id.as_ref()],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
    /// CHECK: validado contra `game.creator`; é para onde o depósito volta,
    /// independentemente de quem acionou o cancelamento.
    #[account(mut, address = game.creator @ EscrowError::CreatorAccountMismatch)]
    pub creator: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct SettleMatch<'info> {
    #[account(mut, address = config.referee @ EscrowError::NotReferee)]
    pub referee: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        close = creator,
        seeds = [b"match", game.match_id.as_ref()],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
    /// CHECK: validado contra `game.creator`; recebe o rent no fechamento.
    #[account(mut, address = game.creator @ EscrowError::CreatorAccountMismatch)]
    pub creator: AccountInfo<'info>,
    /// CHECK: validado contra o argumento `winner` na instrução.
    #[account(mut)]
    pub winner: AccountInfo<'info>,
    #[account(mut, seeds = [b"treasury"], bump = treasury_vault.bump)]
    pub treasury_vault: Account<'info, Vault>,
    #[account(mut, seeds = [b"house"], bump = house_vault.bump)]
    pub house_vault: Account<'info, Vault>,
    #[account(mut, seeds = [b"rank"], bump = rank_vault.bump)]
    pub rank_vault: Account<'info, Vault>,
    /// Registro permanente. Tamanho fixo desde a v3: guarda o hash do replay,
    /// não os bytes. O referee adianta o aluguel e o pote o devolve.
    #[account(
        init,
        payer = referee,
        space = MatchRecordV3::LEN,
        seeds = [b"replay", game.match_id.as_ref()],
        bump
    )]
    pub record: Account<'info, MatchRecordV3>,
    pub system_program: Program<'info, System>,
}

/**
 * Publicar o replay não move dinheiro nem escreve estado, então não pede conta
 * nenhuma além do registro que serve de compromisso.
 *
 * Repare no que NÃO está aqui: nenhum `Signer`. Quem assina é o pagador da
 * transação, quem quer que seja. Era esse o ponto.
 */
#[derive(Accounts)]
pub struct PublishReplay<'info> {
    #[account(seeds = [b"replay", record.match_id.as_ref()], bump = record.bump)]
    pub record: Account<'info, MatchRecordV3>,
}

#[derive(Accounts)]
pub struct ClaimTimeout<'info> {
    /// Qualquer um pode acionar depois do prazo — de propósito.
    pub caller: Signer<'info>,
    #[account(
        mut,
        close = creator,
        seeds = [b"match", game.match_id.as_ref()],
        bump = game.bump
    )]
    pub game: Account<'info, Game>,
    /// CHECK: validado contra `game.creator`.
    #[account(mut, address = game.creator @ EscrowError::CreatorAccountMismatch)]
    pub creator: AccountInfo<'info>,
    /// CHECK: validado contra `game.opponent`.
    #[account(mut, address = game.opponent @ EscrowError::OpponentAccountMismatch)]
    pub opponent: AccountInfo<'info>,
}

/// Cofre do programa. PDA sem chave privada: o saldo só se move pelas regras
/// escritas aqui, e os totais permitem auditar quanto entrou e saiu sem varrer
/// o histórico de transações.
#[account]
pub struct Vault {
    pub kind: u8,
    /// Acumulado histórico recebido.
    pub total_in: u64,
    /// Acumulado histórico retirado (ou queimado, no caso do tesouro).
    pub total_out: u64,
    /// Só o cofre da casa usa. Quantas partidas foram liquidadas.
    pub matches_settled: u64,
    pub bump: u8,
    /**
     * Só o cofre de rank usa. Último `week_id` já pago por
     * `distribute_rank_vault`.
     *
     * É o backstop ON-CHAIN contra reenvio do mesmo pagamento. O servidor
     * (`apps/server/src/rank-distributor.ts`) já tem uma trava em memória
     * que evita reenviar dentro da vida de um processo — mas se o processo
     * REINICIAR bem no meio do estado "pagamento confirmou, gravação do
     * índice falhou", a trava em memória some com ele, e um processo novo
     * pagaria o mesmo `week_id` de novo com o que sobrou no cofre: dinheiro
     * de verdade saindo duas vezes. Este campo fecha essa janela: mora na
     * própria conta, então sobrevive a qualquer restart do servidor.
     * `distribute_rank_vault` recusa um `week_id` que não seja estritamente
     * maior que este.
     */
    pub last_week_id: u32,
}

impl Vault {
    pub const LEN: usize = 8 + 1 + 8 * 3 + 1 + 4;
}

#[derive(Clone, Copy)]
#[repr(u8)]
pub enum VaultKind {
    House = 0,
    Treasury = 1,
    Rank = 2,
}

/**
 * Registro permanente de uma partida liquidada.
 *
 * Guarda o COMPROMISSO com o replay, não o replay.
 *
 * Até a v2 os bytes inteiros ficavam aqui, e a promessa era "daqui a anos, com
 * o site fora do ar, os bytes continuam lá". Era uma promessa cara: a Solana
 * cobra 6960 lamports por byte de estado permanente, e cobra por transmissão
 * uma taxa fixa que não olha o tamanho. Medido: uma transação carregando 200
 * bytes custa MENOS que armazenar um único byte.
 *
 * O que a v3 troca:
 *
 *   guardar os bytes (516)  →  0,00568 SOL, e crescendo com a partida
 *   guardar o hash deles    →  0,00231 SOL, fixo
 *
 * 2,5× — não as duas ordens de grandeza que a taxa de transação sugeria. A
 * diferença é que toda conta paga 128 bytes de sobrecarga antes do primeiro
 * byte útil, e o registro ainda carrega duas chaves, dois nonces e o hash. O
 * replay era a maior parte, não a totalidade.
 *
 * O que NÃO muda: ninguém consegue adulterar um replay sem que o hash denuncie.
 * A integridade continua garantida pelo protocolo.
 *
 * O que muda: a DISPONIBILIDADE dos bytes deixa de ser problema da Solana e
 * passa a ser nosso. Eles vivem no servidor e, principalmente, nos dois
 * clientes — cada jogador termina a partida com a cópia inteira, então o
 * perdedor não precisa da nossa boa vontade para contestar.
 */
#[account]
pub struct MatchRecordV3 {
    pub match_id: [u8; 16],
    pub winner: Pubkey,
    pub loser: Pubkey,
    /**
     * O criador da mesa é o VENCEDOR?
     *
     * O jogador 0 do replay é sempre o criador, e sem essa ligação o replay
     * era inauditável no ponto que mais importa: ele chama os jogadores de 0 e
     * 1, e a conta `Game`, que tinha o criador, é fechada na liquidação. Um
     * referee comprometido pagava o perdedor e a auditoria dizia "confere".
     *
     * Guardar a chave inteira seria redundante: o criador é necessariamente um
     * dos dois acima. Um bit basta, e economiza 32 bytes de estado permanente
     * em toda partida.
     */
    pub creator_won: bool,
    pub pot: u64,
    pub settled_at: i64,
    /**
     * Os nonces revelados, que geraram o seed da quebra.
     *
     * Ficam aqui para a auditoria ser completa sem depender da conta `Game`,
     * que é fechada: qualquer pessoa confere que
     * `seed == sha256(nonce_creator ‖ nonce_opponent)` e que cada nonce bate
     * com o compromisso que o contrato validou na liquidação.
     */
    pub nonce_creator: [u8; 32],
    pub nonce_opponent: [u8; 32],
    /**
     * SHA-256 do replay.
     *
     * É o que amarra os bytes servidos fora da chain a esta partida. Quem
     * recebe um replay confere o hash contra este campo; se bater, os bytes são
     * exatamente os que o referee afirmou ao liquidar, e reproduzi-los prova o
     * vencedor. Se não bater, o replay é falso — e o contrário também vale:
     * ninguém consegue trocar o replay depois sem que isto denuncie.
     */
    pub replay_hash: [u8; 32],
    /**
     * Tamanho do replay em bytes.
     *
     * Barato (2 bytes) e evita um ataque de disponibilidade parcial: sem ele,
     * quem serve os bytes pode entregar um prefixo e alegar que é tudo. Com o
     * tamanho gravado, um replay truncado é detectável antes mesmo de hashear.
     */
    pub replay_len: u16,
    pub bump: u8,
}

impl MatchRecordV3 {
    /// Tamanho fixo — o registro não cresce mais com a partida.
    pub const LEN: usize = 8 + 16 + 32 * 2 + 1 + 8 + 8 + 32 * 2 + 32 + 2 + 1;
}

/**
 * Procedência de uma versão da física.
 *
 * Uma conta por versão. Quem tem um replay lê a versão dele, busca a conta
 * correspondente, e obtém tudo o que precisa para verificar por conta própria.
 */
#[account]
pub struct Provenance {
    pub engine_version: u16,
    /// Impressão digital do comportamento, em ASCII (ex.: "1751bd8c").
    pub physics_digest: [u8; 8],
    /// SHA-256 do documento de especificação.
    pub spec_hash: [u8; 32],
    /// Onde o documento está arquivado de forma permanente.
    pub spec_uri: String,
    pub published_at: i64,
    pub bump: u8,
}

impl Provenance {
    pub const BASE_LEN: usize = 8 + 2 + 8 + 32 + 4 + 8 + 1;

    pub fn space(uri_len: usize) -> usize {
        Self::BASE_LEN + uri_len
    }
}

#[derive(Accounts)]
#[instruction(engine_version: u16, physics_digest: [u8; 8], spec_hash: [u8; 32], spec_uri: String)]
pub struct PublishProvenance<'info> {
    #[account(mut, address = config.authority @ EscrowError::NotAuthority)]
    pub authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    /// Uma conta por versão: a v2 nasce ao lado da v1, sem apagá-la.
    #[account(
        init,
        payer = authority,
        space = Provenance::space(spec_uri.len()),
        seeds = [b"provenance".as_ref(), engine_version.to_le_bytes().as_ref()],
        bump
    )]
    pub provenance: Account<'info, Provenance>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct MigrateConfig<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    /// CHECK: discriminador e autoridade conferidos dentro da instrução; não
    /// dá para usar `Account<Config>` aqui porque a conta ainda tem o tamanho
    /// antigo e a desserialização falharia antes do realloc.
    #[account(mut, seeds = [b"config"], bump)]
    pub config: UncheckedAccount<'info>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitVaults<'info> {
    #[account(mut, address = config.authority @ EscrowError::NotAuthority)]
    pub authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = authority,
        space = Vault::LEN,
        seeds = [b"house"],
        bump
    )]
    pub house_vault: Account<'info, Vault>,
    #[account(
        init,
        payer = authority,
        space = Vault::LEN,
        seeds = [b"treasury"],
        bump
    )]
    pub treasury_vault: Account<'info, Vault>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct InitRankVault<'info> {
    #[account(mut, address = config.authority @ EscrowError::NotAuthority)]
    pub authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = authority,
        space = Vault::LEN,
        seeds = [b"rank"],
        bump
    )]
    pub rank_vault: Account<'info, Vault>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
#[instruction(top5: [Pubkey; 5], amounts: [u64; 5], week_id: u32)]
pub struct DistributeRankVault<'info> {
    #[account(address = config.rank_authority @ EscrowError::NotRankAuthority)]
    pub rank_authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"rank"], bump = rank_vault.bump)]
    pub rank_vault: Account<'info, Vault>,
    /// CHECK: validado contra `top5[0]` na constraint acima.
    #[account(mut, address = top5[0])]
    pub recipient0: AccountInfo<'info>,
    /// CHECK: validado contra `top5[1]`.
    #[account(mut, address = top5[1])]
    pub recipient1: AccountInfo<'info>,
    /// CHECK: validado contra `top5[2]`.
    #[account(mut, address = top5[2])]
    pub recipient2: AccountInfo<'info>,
    /// CHECK: validado contra `top5[3]`.
    #[account(mut, address = top5[3])]
    pub recipient3: AccountInfo<'info>,
    /// CHECK: validado contra `top5[4]`.
    #[account(mut, address = top5[4])]
    pub recipient4: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct WithdrawHouse<'info> {
    #[account(address = config.authority @ EscrowError::NotAuthority)]
    pub authority: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(mut, seeds = [b"house"], bump = house_vault.bump)]
    pub house_vault: Account<'info, Vault>,
    /// CHECK: destino escolhido pela autoridade; não há o que validar aqui.
    #[account(mut)]
    pub destination: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct BurnTreasury<'info> {
    /// Qualquer um pode acionar a queima — ela não beneficia quem chama.
    pub caller: Signer<'info>,
    #[account(mut, seeds = [b"treasury"], bump = treasury_vault.bump)]
    pub treasury_vault: Account<'info, Vault>,
    /// CHECK: comparado com o incinerador oficial dentro da instrução.
    #[account(mut)]
    pub incinerator: AccountInfo<'info>,
}

#[derive(Accounts)]
pub struct AdminOnly<'info> {
    #[account(address = config.authority @ EscrowError::NotAuthority)]
    pub authority: Signer<'info>,
    #[account(mut, seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
}

// ---------------------------------------------------------------- eventos

#[event]
pub struct MatchSettled {
    pub match_id: [u8; 16],
    pub winner: Pubkey,
    pub pot: u64,
    pub prize: u64,
    pub treasury: u64,
    pub house: u64,
    pub rank: u64,
    /// Hash do replay. Repete o que fica na conta, para quem escuta a chain não
    /// precisar buscá-la.
    pub result_hash: [u8; 32],
}

/**
 * Os bytes do replay desta partida foram publicados na chain.
 *
 * O evento carrega só o identificador: os BYTES estão nos dados da transação
 * que o emitiu, e duplicá-los no log dobraria o custo sem provar nada a mais.
 * Serve para um indexador achar a transação sem varrer o histórico inteiro.
 */
#[event]
pub struct ReplayPublished {
    pub match_id: [u8; 16],
    pub replay_len: u16,
}

#[event]
pub struct ProvenancePublished {
    pub engine_version: u16,
    pub physics_digest: [u8; 8],
    pub spec_hash: [u8; 32],
}

#[event]
pub struct SplitsChanged {
    pub winner_bps: u16,
    pub house_bps: u16,
    pub protocol_bps: u16,
    pub rank_bps: u16,
}

#[event]
pub struct RankAuthorityChanged {
    pub rank_authority: Pubkey,
}

#[event]
pub struct RankVaultDistributed {
    pub week_id: u32,
    pub top5: [Pubkey; 5],
    pub amounts: [u64; 5],
    pub total: u64,
}

#[event]
pub struct HouseWithdrawn {
    pub amount: u64,
    pub destination: Pubkey,
    pub total_out: u64,
}

#[event]
pub struct TreasuryBurned {
    pub amount: u64,
    pub total_burned: u64,
}

/// A partida ficou com os dois depósitos e o relógio dela começou.
#[event]
pub struct MatchCommitted {
    pub match_id: [u8; 16],
    pub deadline: i64,
}

/**
 * A configuração de operação mudou.
 *
 * A troca de referee é a mudança mais perigosa que a autoridade pode fazer —
 * quem assina as liquidações passa a ser outro. Era a única que não deixava
 * rastro em evento, enquanto `set_splits` e os saques deixavam.
 */
#[event]
pub struct ConfigChanged {
    pub referee: Pubkey,
    pub min_stake: u64,
    pub max_stake: u64,
}

/// A autoridade do programa mudou de mãos.
#[event]
pub struct AuthorityChanged {
    pub previous: Pubkey,
    pub current: Pubkey,
}

#[event]
pub struct MatchRefunded {
    pub match_id: [u8; 16],
    pub stake: u64,
}

// ---------------------------------------------------------------- erros


// ------------------------------------------------------- torneio: contas

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TournamentState {
    Open = 0,
    Playing = 1,
    Settled = 2,
    Cancelled = 3,
}

/**
 * O torneio e o cofre dele.
 *
 * Os depositos ficam NESTA conta, como os do `Game` ficam na dele: nao ha PDA
 * de cofre separada, e por isso nao ha um passo a mais para esquecer.
 *
 * Os inscritos NAO moram aqui, num vetor: cada um tem a propria
 * `TournamentEntry`. Um vetor de 16 chaves engordaria a conta em 512 bytes e
 * ainda precisaria de uma busca para garantir uma inscricao por pessoa -- o
 * que a PDA da entry garante de graca, pelo runtime.
 */
#[account]
pub struct Tournament {
    pub tournament_id: [u8; 16],
    /// Quem abriu a sala. Nao joga por isso e nao recebe nada por isso.
    pub creator: Pubkey,
    /// A inscricao, em lamports. Paga uma vez, no registro.
    pub stake: u64,
    /// O acumulado ja depositado. Guardado, e nao `stake * joined`, porque
    /// uma devolucao individual tira do pote sem tirar vaga do passado.
    pub pot: u64,
    /// Vagas da chave: 4, 8 ou 16.
    pub seats: u8,
    /// Quantos ja depositaram.
    pub joined: u8,
    pub state: u8,
    /// Ate quando aceita inscricao. Depois disto, qualquer um pode cancelar.
    pub deadline: i64,
    pub bump: u8,
}

impl Tournament {
    pub const LEN: usize = 8 + 16 + 32 + 8 + 8 + 1 + 1 + 1 + 8 + 1;
}

/// Uma inscricao paga. A existencia dela E a prova do deposito.
#[account]
pub struct TournamentEntry {
    pub tournament: Pubkey,
    pub player: Pubkey,
    pub bump: u8,
}

impl TournamentEntry {
    pub const LEN: usize = 8 + 32 + 32 + 1;
}

// --------------------------------------------------- torneio: contextos

#[derive(Accounts)]
#[instruction(tournament_id: [u8; 16])]
pub struct CreateTournament<'info> {
    #[account(mut)]
    pub creator: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        init,
        payer = creator,
        space = Tournament::LEN,
        seeds = [b"tournament", tournament_id.as_ref()],
        bump
    )]
    pub tournament: Account<'info, Tournament>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct JoinTournament<'info> {
    #[account(mut)]
    pub player: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [b"tournament", tournament.tournament_id.as_ref()],
        bump = tournament.bump
    )]
    pub tournament: Account<'info, Tournament>,
    /// Uma por (torneio, jogador): `init` recusa a segunda inscricao.
    #[account(
        init,
        payer = player,
        space = TournamentEntry::LEN,
        seeds = [b"entry", tournament.key().as_ref(), player.key().as_ref()],
        bump
    )]
    pub entry: Account<'info, TournamentEntry>,
    pub system_program: Program<'info, System>,
}

#[derive(Accounts)]
pub struct SettleTournament<'info> {
    /// So o referee declara o campeao, como declara o vencedor da mesa.
    #[account(mut, address = config.referee @ EscrowError::NotReferee)]
    pub referee: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [b"tournament", tournament.tournament_id.as_ref()],
        bump = tournament.bump
    )]
    pub tournament: Account<'info, Tournament>,
    /// CHECK: o endereco e conferido contra o argumento `champion`.
    #[account(mut)]
    pub champion: AccountInfo<'info>,
    #[account(mut, seeds = [b"treasury"], bump = treasury_vault.bump)]
    pub treasury_vault: Account<'info, Vault>,
    #[account(mut, seeds = [b"house"], bump = house_vault.bump)]
    pub house_vault: Account<'info, Vault>,
    #[account(mut, seeds = [b"rank"], bump = rank_vault.bump)]
    pub rank_vault: Account<'info, Vault>,
}

#[derive(Accounts)]
pub struct CancelTournament<'info> {
    /// Qualquer um depois do prazo; o referee a qualquer momento. Quem aciona
    /// nao escolhe destino: isto so muda o estado.
    pub caller: Signer<'info>,
    #[account(seeds = [b"config"], bump = config.bump)]
    pub config: Account<'info, Config>,
    #[account(
        mut,
        seeds = [b"tournament", tournament.tournament_id.as_ref()],
        bump = tournament.bump
    )]
    pub tournament: Account<'info, Tournament>,
}

#[derive(Accounts)]
pub struct ClaimRefund<'info> {
    /// O destino e o jogador da `entry`, e o `address =` o prende: quem
    /// aciona nao escolhe para quem o dinheiro vai.
    #[account(mut, address = entry.player @ EscrowError::NotEntryOwner)]
    pub player: Signer<'info>,
    #[account(
        mut,
        seeds = [b"tournament", tournament.tournament_id.as_ref()],
        bump = tournament.bump
    )]
    pub tournament: Account<'info, Tournament>,
    /// `close` devolve o aluguel ao jogador e impede o segundo saque.
    #[account(
        mut,
        close = player,
        seeds = [b"entry", tournament.key().as_ref(), player.key().as_ref()],
        bump = entry.bump,
        constraint = entry.tournament == tournament.key() @ EscrowError::NotEntryOwner
    )]
    pub entry: Account<'info, TournamentEntry>,
}

// ----------------------------------------------------- torneio: eventos

#[event]
pub struct TournamentCreated {
    pub tournament_id: [u8; 16],
    pub creator: Pubkey,
    pub stake: u64,
    pub seats: u8,
}

#[event]
pub struct TournamentJoined {
    pub tournament_id: [u8; 16],
    pub player: Pubkey,
    pub joined: u8,
    pub pot: u64,
}

#[event]
pub struct TournamentSettled {
    pub tournament_id: [u8; 16],
    pub champion: Pubkey,
    pub pot: u64,
    pub prize: u64,
}

#[event]
pub struct TournamentCancelled {
    pub tournament_id: [u8; 16],
    pub pot: u64,
}

#[event]
pub struct TournamentRefunded {
    pub tournament_id: [u8; 16],
    pub player: Pubkey,
    pub amount: u64,
}

#[error_code]
pub enum EscrowError {
    #[msg("Faixa de valores inválida.")]
    InvalidStakeRange,
    #[msg("Valor de entrada fora dos limites.")]
    StakeOutOfRange,
    #[msg("Prazo inválido.")]
    InvalidTimeout,
    #[msg("O programa está pausado.")]
    Paused,
    #[msg("Esta mesa não aceita mais jogadores.")]
    NotJoinable,
    #[msg("Você não pode entrar na própria mesa.")]
    CannotJoinOwnMatch,
    #[msg("Esta mesa expirou.")]
    MatchExpired,
    #[msg("Só é possível cancelar antes de alguém entrar.")]
    NotCancellable,
    #[msg("Esta partida não está em estado de liquidação.")]
    NotSettleable,
    #[msg("O pote não cobre o aluguel do registro permanente do replay.")]
    PotTooSmallForRecord,
    #[msg("O nonce revelado não corresponde ao compromisso feito no depósito.")]
    BadReveal,
    #[msg("O vencedor não é um dos jogadores desta partida.")]
    WinnerNotInMatch,
    #[msg("A conta do vencedor não confere com o vencedor declarado.")]
    WinnerAccountMismatch,
    #[msg("A conta do criador não confere.")]
    CreatorAccountMismatch,
    #[msg("A conta do oponente não confere.")]
    OpponentAccountMismatch,
    #[msg("Conta de tesouro inválida.")]
    TreasuryMismatch,
    #[msg("Conta da casa inválida.")]
    HouseMismatch,
    #[msg("Apenas o criador pode fazer isso.")]
    NotCreator,
    #[msg("Apenas o referee pode liquidar.")]
    NotReferee,
    #[msg("Apenas a autoridade pode fazer isso.")]
    NotAuthority,
    #[msg("O prazo ainda não estourou.")]
    NotExpiredYet,
    #[msg("Esta partida não é reembolsável.")]
    NotRefundable,
    #[msg("Saldo insuficiente no escrow.")]
    InsufficientEscrowBalance,
    #[msg("Saldo insuficiente no cofre.")]
    InsufficientVaultBalance,
    #[msg("Apenas a autoridade do cofre de rank pode distribuí-lo.")]
    NotRankAuthority,
    #[msg("A soma dos valores a distribuir excede o saldo do cofre de rank.")]
    RankAmountsExceedVault,
    #[msg("A conta informada não é o incinerador da Solana.")]
    NotIncinerator,
    #[msg("A conta informada não é o Config deste programa.")]
    NotConfigAccount,
    #[msg("O replay passa do tamanho máximo permitido.")]
    ReplayTooLarge,
    #[msg("Endereço da especificação vazio ou longo demais.")]
    SpecUriTooLong,
    #[msg("As fatias precisam somar exatamente 100%.")]
    SplitsDoNotSum,
    #[msg("A fatia do vencedor está abaixo do mínimo permitido pelo programa.")]
    WinnerShareTooLow,
    #[msg("Estouro aritmético.")]
    MathOverflow,
    #[msg("O replay publicado tem comprimento diferente do gravado na liquidação.")]
    ReplayLenMismatch,
    #[msg("O replay publicado não bate com o hash gravado na liquidação.")]
    ReplayHashMismatch,
    #[msg("Esta semana do cofre de rank já foi distribuída.")]
    RankWeekAlreadyDistributed,
    #[msg("Numero de vagas invalido: 2, 4, 8 ou 16.")]
    BadSeats,
    #[msg("Torneio cheio.")]
    TournamentFull,
    #[msg("O prazo de inscricao passou.")]
    DeadlinePassed,
    #[msg("Esta inscricao nao e sua.")]
    NotEntryOwner,
}
