//! Plays whole rounds in LiteSVM. The clock is moved forward by hand, so the real
//! timings (hourly closes, 3 min reveal) cost nothing.

use {
    anchor_lang::{
        prelude::{AccountMeta, Clock, Pubkey},
        solana_program::{instruction::Instruction, system_program},
        AccountDeserialize, InstructionData, ToAccountMetas,
    },
    anchor_spl::token::ID as TOKEN_PROGRAM,
    litesvm::LiteSVM,
    solana_account::Account,
    solana_keypair::Keypair,
    solana_message::{Message, VersionedMessage},
    solana_sha256_hasher::hashv,
    solana_signer::Signer,
    solana_transaction::versioned::VersionedTransaction,
    unique::{Bundle, ConfigParams, Entry, PlayerProfile, Round, StakePool, Staker, Table, TableParams, Tally, BACKUP_LEN, REPICK_CUTOFF_SECS as REPICK_CUTOFF},
};

const DAY: i64 = 24 * 60 * 60;

const SOL: u64 = 1_000_000_000;
const DEC: u64 = 1_000_000; // 6 decimals
const HOUR: i64 = 60 * 60;
const REVEAL: i64 = 3 * 60;
const CLAIM: i64 = 30;
const DEPOSIT: u64 = SOL / 100;
/// A whole hour (1_800_000_000 = 500_000 × 3600), so rounds entered at START close at START + HOUR.
const START: i64 = 1_800_000_000;
const CLOSE: i64 = START + HOUR;
const ALL_CLOSED: i64 = CLOSE + REVEAL + CLAIM;
/// The launch split: 100% / 85-15 / 75-15-10 / 70-15-10-5.
const SPLITS: [[u16; 4]; 4] = [[10000, 0, 0, 0], [8500, 1500, 0, 0], [7500, 1500, 1000, 0], [7000, 1500, 1000, 500]];

/// A round: (table id, round id). Most tests play table 0.
type R = (u8, u64);
const R1: R = (0, 1);
const R2: R = (0, 2);

fn tok(n: u64) -> u64 { n * DEC }
fn pda(seeds: &[&[u8]]) -> Pubkey { Pubkey::find_program_address(seeds, &unique::ID).0 }
fn config_pda() -> Pubkey { pda(&[b"config"]) }
fn vault_pda() -> Pubkey { pda(&[b"vault"]) }
fn reserve_pda() -> Pubkey { pda(&[b"reserve"]) }
fn stake_vault_pda() -> Pubkey { pda(&[b"stake_vault"]) }
fn stake_pool_pda() -> Pubkey { pda(&[b"stake_pool"]) }
fn staker_pda(owner: &Pubkey) -> Pubkey { pda(&[b"stake", owner.as_ref()]) }
fn profile_pda(player: &Pubkey) -> Pubkey { pda(&[b"player", player.as_ref()]) }
fn table_pda(t: u8) -> Pubkey { pda(&[b"table", &[t]]) }
fn round_pda(r: R) -> Pubkey { pda(&[b"round", &[r.0], &r.1.to_le_bytes()]) }
fn entry_pda(r: R, c: &[u8; 32]) -> Pubkey { pda(&[b"entry", round_pda(r).as_ref(), c]) }
fn tally_pda(r: R, n: u64) -> Pubkey { pda(&[b"num", round_pda(r).as_ref(), &n.to_le_bytes()]) }

/// A sealed pick, built exactly like the site's lib/commit.ts.
#[derive(Clone, Copy)]
struct Sealed { n: u64, salt: [u8; 32], commitment: [u8; 32] }

fn seal(n: u64, player: &Pubkey) -> Sealed {
    let salt = Keypair::new().pubkey().to_bytes(); // any 32 random bytes
    let commitment = hashv(&[&n.to_le_bytes(), &salt, player.as_ref()]).to_bytes();
    Sealed { n, salt, commitment }
}

fn bundle_pda(r: R, c: &[u8; 32]) -> Pubkey { pda(&[b"bundle", round_pda(r).as_ref(), c]) }

/// A sealed bundle: the numbers one after another, then the salt and the player.
#[derive(Clone)]
struct SealedBundle { ns: Vec<u64>, salt: [u8; 32], commitment: [u8; 32] }

fn seal_bundle(ns: &[u64], player: &Pubkey) -> SealedBundle {
    let salt = Keypair::new().pubkey().to_bytes();
    let packed: Vec<u8> = ns.iter().flat_map(|n| n.to_le_bytes()).collect();
    let commitment = hashv(&[&packed, &salt, player.as_ref()]).to_bytes();
    SealedBundle { ns: ns.to_vec(), salt, commitment }
}

/// A table: $5 tickets, a round every hour, closing at `offset` past the hour.
fn hourly(offset: i64) -> TableParams {
    TableParams {
        ticket_tokens: tok(5), period_secs: HOUR, close_offset_secs: offset,
        reveal_secs: REVEAL, claim_secs: CLAIM, reveal_deposit_lamports: DEPOSIT,
    }
}

struct Player { kp: Keypair, ata: Pubkey }

struct Game {
    svm: LiteSVM,
    admin: Keypair,
    keeper: Keypair,
    mint: Pubkey,
    team_wallet: Pubkey,
    team_token: Pubkey,
}

impl Game {
    /// Table 0 closes hourly at :00. `tiers`: entries needed for 2, 3 and 4 winners.
    fn new(tiers: [u64; 3]) -> Self {
        // LiteSVM 0.10 predates the SBPF v3 format `anchor build` produces, so tests load a
        // v2 build of the same code: `cargo build-sbf --arch v2 --sbf-out-dir target/deploy-test`
        // (the `test` script in Anchor.toml does this).
        let so = concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/deploy-test/unique.so");
        let mut svm = LiteSVM::new();
        svm.add_program(unique::ID, &std::fs::read(so).expect("build the test program first, see comment above"))
            .unwrap();
        let admin = Keypair::new();
        svm.airdrop(&admin.pubkey(), 100 * SOL).unwrap();
        let mut g = Game { svm, admin, keeper: Keypair::new(), mint: Pubkey::new_unique(), team_wallet: Pubkey::new_unique(), team_token: Pubkey::default() };
        let keeper = g.keeper.pubkey();
        g.svm.airdrop(&keeper, SOL).unwrap();
        g.set_mint_supply(0);
        g.team_token = g.token_account(g.team_wallet, 0);
        g.warp_to(START);
        let p = g.params(tiers);
        g.send(&[g.ix_initialize(p)], &[]).unwrap();
        g.create_table(0, hourly(0)).unwrap();
        g
    }

    fn params(&self, tiers: [u64; 3]) -> ConfigParams {
        ConfigParams {
            pool_bps: 8500,
            reserve_bps: 500,
            burn_bps: 500,
            referral_bps: 100,
            tier_min_entries: tiers,
            tier_splits: SPLITS,
            team_wallet: self.team_wallet,
            keeper: self.keeper.pubkey(),
            max_price_change_bps: 2500,
            min_stake_tokens: tok(1),
            backlog_release_secs: 30 * DAY,
        }
    }

    // --- raw SPL token accounts (packed by hand, no extra crates) ---------------

    fn set_mint_supply(&mut self, supply: u64) {
        let mut d = vec![0u8; 82];
        d[0..4].copy_from_slice(&1u32.to_le_bytes());
        d[4..36].copy_from_slice(self.admin.pubkey().as_ref());
        d[36..44].copy_from_slice(&supply.to_le_bytes());
        d[44] = 6; // decimals
        d[45] = 1; // initialized
        self.put(self.mint, d);
    }

    fn token_account(&mut self, owner: Pubkey, amount: u64) -> Pubkey {
        let key = Pubkey::new_unique();
        let mut d = vec![0u8; 165];
        d[0..32].copy_from_slice(self.mint.as_ref());
        d[32..64].copy_from_slice(owner.as_ref());
        d[64..72].copy_from_slice(&amount.to_le_bytes());
        d[108] = 1; // initialized
        self.put(key, d);
        let supply = self.supply();
        self.set_mint_supply(supply + amount);
        key
    }

    fn put(&mut self, key: Pubkey, data: Vec<u8>) {
        let lamports = self.svm.minimum_balance_for_rent_exemption(data.len());
        self.svm.set_account(key, Account { lamports, data, owner: TOKEN_PROGRAM, executable: false, rent_epoch: 0 }).unwrap();
    }

    fn balance(&self, token_account: &Pubkey) -> u64 {
        let d = self.svm.get_account(token_account).unwrap().data;
        u64::from_le_bytes(d[64..72].try_into().unwrap())
    }
    fn supply(&self) -> u64 {
        self.svm.get_account(&self.mint).map(|a| u64::from_le_bytes(a.data[36..44].try_into().unwrap())).unwrap_or(0)
    }
    fn lamports(&self, key: &Pubkey) -> u64 { self.svm.get_account(key).map(|a| a.lamports).unwrap_or(0) }

    fn player(&mut self, tokens: u64) -> Player {
        let kp = Keypair::new();
        self.svm.airdrop(&kp.pubkey(), 2 * SOL).unwrap();
        let ata = self.token_account(kp.pubkey(), tok(tokens));
        Player { kp, ata }
    }
    fn players<const N: usize>(&mut self) -> [Player; N] { std::array::from_fn(|_| self.player(100)) }

    // --- clock & state ---------------------------------------------------------

    fn warp_to(&mut self, t: i64) {
        let mut c = self.svm.get_sysvar::<Clock>();
        c.unix_timestamp = t;
        self.svm.set_sysvar(&c);
    }
    fn fetch<T: AccountDeserialize>(&self, key: &Pubkey) -> T {
        T::try_deserialize(&mut &self.svm.get_account(key).unwrap().data[..]).unwrap()
    }
    fn round(&self, r: R) -> Round { self.fetch(&round_pda(r)) }
    fn table(&self, t: u8) -> Table { self.fetch(&table_pda(t)) }
    fn tally(&self, r: R, n: u64) -> Tally { self.fetch(&tally_pda(r, n)) }
    fn staker(&self, p: &Player) -> Staker { self.fetch(&staker_pda(&p.kp.pubkey())) }
    fn stake_pool(&self) -> StakePool { self.fetch(&stake_pool_pda()) }
    /// Claimed numbers, lowest first.
    fn claimed(&self, r: R) -> Vec<u64> {
        self.round(r).claims.iter().map(|c| c.number).filter(|&n| n != u64::MAX).collect()
    }

    /// Sends with the admin as fee payer. Errors come back as the program logs, so tests
    /// can check for the error name.
    /// Like `send`; returns the compute units used.
    fn send_cu(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> u64 {
        self.svm.expire_blockhash();
        let mut all: Vec<&Keypair> = vec![&self.admin];
        all.extend_from_slice(signers);
        let msg = Message::new_with_blockhash(ixs, Some(&self.admin.pubkey()), &self.svm.latest_blockhash());
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &all).unwrap();
        self.svm.send_transaction(tx).map_err(|e| format!("{:?}
{}", e.err, e.meta.logs.join("
"))).unwrap().compute_units_consumed
    }
    fn send(&mut self, ixs: &[Instruction], signers: &[&Keypair]) -> Result<(), String> {
        self.svm.expire_blockhash();
        let mut all: Vec<&Keypair> = vec![&self.admin];
        all.extend_from_slice(signers);
        let msg = Message::new_with_blockhash(ixs, Some(&self.admin.pubkey()), &self.svm.latest_blockhash());
        let tx = VersionedTransaction::try_new(VersionedMessage::Legacy(msg), &all).unwrap();
        self.svm.send_transaction(tx).map(|_| ()).map_err(|e| format!("{:?}\n{}", e.err, e.meta.logs.join("\n")))
    }

    // --- instructions ----------------------------------------------------------

    fn ix(data: Vec<u8>, accounts: impl ToAccountMetas) -> Instruction {
        Instruction::new_with_bytes(unique::ID, &data, accounts.to_account_metas(None))
    }

    fn ix_initialize(&self, p: ConfigParams) -> Instruction {
        Self::ix(unique::instruction::Initialize { p }.data(), unique::accounts::Initialize {
            admin: self.admin.pubkey(), config: config_pda(), mint: self.mint, vault: vault_pda(), reserve: reserve_pda(),
            stake_vault: stake_vault_pda(), stake_pool: stake_pool_pda(),
            team_token: self.team_token, token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_update_config(&self, admin: Pubkey, p: ConfigParams) -> Instruction {
        Self::ix(unique::instruction::UpdateConfig { p }.data(), unique::accounts::UpdateConfig {
            admin, config: config_pda(), mint: self.mint, team_token: self.team_token,
        })
    }
    fn ix_create_table(&self, admin: Pubkey, id: u8, p: TableParams) -> Instruction {
        Self::ix(unique::instruction::CreateTable { id, p }.data(), unique::accounts::CreateTable {
            admin, config: config_pda(), table: table_pda(id), round: round_pda((id, 1)), system_program: system_program::ID,
        })
    }
    fn ix_update_table(&self, admin: Pubkey, t: u8, p: TableParams) -> Instruction {
        Self::ix(unique::instruction::UpdateTable { p }.data(), unique::accounts::UpdateTable {
            admin, config: config_pda(), table: table_pda(t),
        })
    }
    fn ix_set_table_enabled(&self, admin: Pubkey, t: u8, enabled: bool) -> Instruction {
        let current = self.table(t).current_round;
        Self::ix(unique::instruction::SetTableEnabled { enabled }.data(), unique::accounts::SetTableEnabled {
            admin, config: config_pda(), table: table_pda(t), round: round_pda((t, current)),
        })
    }
    fn ix_enter(&self, p: &Player, r: R, s: &Sealed) -> Instruction { self.ix_enter_ref(p, r, s, None) }
    fn ix_enter_ref(&self, p: &Player, r: R, s: &Sealed, referrer_token: Option<Pubkey>) -> Instruction { self.ix_enter_max(p, r, s, referrer_token, u64::MAX) }
    /// A pick that pays at most `max_ticket` (the price the player saw).
    fn ix_enter_max(&self, p: &Player, r: R, s: &Sealed, referrer_token: Option<Pubkey>, max_ticket: u64) -> Instruction {
        Self::ix(unique::instruction::Enter { commitment: s.commitment, backup: [7; BACKUP_LEN], max_ticket }.data(), unique::accounts::Enter {
            player: p.kp.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), entry: entry_pda(r, &s.commitment),
            mint: self.mint, player_token: p.ata, vault: vault_pda(), reserve: reserve_pda(), stake_pool: stake_pool_pda(),
            team_token: self.team_token, referrer_token, profile: profile_pda(&p.kp.pubkey()), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_enter_from_stake(&self, p: &Player, r: R, s: &Sealed) -> Instruction { self.ix_enter_from_stake_ref(p, r, s, None) }
    fn ix_enter_from_stake_ref(&self, p: &Player, r: R, s: &Sealed, referrer_token: Option<Pubkey>) -> Instruction { self.ix_enter_from_stake_max(p, r, s, referrer_token, u64::MAX) }
    fn ix_enter_from_stake_max(&self, p: &Player, r: R, s: &Sealed, referrer_token: Option<Pubkey>, max_ticket: u64) -> Instruction {
        Self::ix(unique::instruction::EnterFromStake { commitment: s.commitment, backup: [7; BACKUP_LEN], max_ticket }.data(), unique::accounts::EnterFromStake {
            player: p.kp.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), entry: entry_pda(r, &s.commitment),
            staker: staker_pda(&p.kp.pubkey()), stake_pool: stake_pool_pda(), mint: self.mint, player_token: p.ata,
            stake_vault: stake_vault_pda(), vault: vault_pda(), reserve: reserve_pda(), team_token: self.team_token,
            referrer_token, profile: profile_pda(&p.kp.pubkey()), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_repick(&self, signer: &Player, r: R, old: &Sealed, new: &Sealed) -> Instruction {
        Self::ix(unique::instruction::Repick { new_commitment: new.commitment, new_backup: [9; BACKUP_LEN] }.data(), unique::accounts::Repick {
            player: signer.kp.pubkey(), round: round_pda(r),
            old_entry: entry_pda(r, &old.commitment), new_entry: entry_pda(r, &new.commitment), system_program: system_program::ID,
        })
    }
    fn ix_stake(&self, p: &Player, amount: u64) -> Instruction {
        Self::ix(unique::instruction::Stake { amount }.data(), unique::accounts::Stake {
            owner: p.kp.pubkey(), config: config_pda(), stake_pool: stake_pool_pda(), staker: staker_pda(&p.kp.pubkey()),
            mint: self.mint, owner_token: p.ata, stake_vault: stake_vault_pda(), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_unstake(&self, p: &Player, amount: u64) -> Instruction {
        Self::ix(unique::instruction::Unstake { amount }.data(), unique::accounts::Unstake {
            owner: p.kp.pubkey(), config: config_pda(), stake_pool: stake_pool_pda(), staker: staker_pda(&p.kp.pubkey()),
            mint: self.mint, owner_token: p.ata, stake_vault: stake_vault_pda(), token_program: TOKEN_PROGRAM,
        })
    }
    fn ix_claim_rewards(&self, p: &Player) -> Instruction {
        Self::ix(unique::instruction::ClaimRewards {}.data(), unique::accounts::ClaimRewards {
            owner: p.kp.pubkey(), config: config_pda(), stake_pool: stake_pool_pda(), staker: staker_pda(&p.kp.pubkey()),
            mint: self.mint, owner_token: p.ata, reserve: reserve_pda(), token_program: TOKEN_PROGRAM,
        })
    }
    fn ix_set_ticket(&self, signer: Pubkey, t: u8, ticket_tokens: u64) -> Instruction {
        let current = self.table(t).current_round;
        Self::ix(unique::instruction::SetTicket { ticket_tokens }.data(), unique::accounts::SetTicket { signer, config: config_pda(), table: table_pda(t), round: round_pda((t, current)) })
    }
    fn ix_start_close(&self, t: u8) -> Instruction {
        let current = self.table(t).current_round;
        Self::ix(unique::instruction::StartClose {}.data(), unique::accounts::StartClose { table: table_pda(t), round: round_pda((t, current)) })
    }
    fn ix_set_paused(&self, admin: Pubkey, paused: bool) -> Instruction {
        Self::ix(unique::instruction::SetPaused { paused }.data(), unique::accounts::AdminOnly { admin, config: config_pda() })
    }
    fn ix_reveal(&self, p: &Player, r: R, s: &Sealed, n: u64, salt: [u8; 32]) -> Instruction {
        Self::ix(unique::instruction::Reveal { number: n, salt }.data(), unique::accounts::Reveal {
            payer: p.kp.pubkey(), round: round_pda(r), entry: entry_pda(r, &s.commitment),
            player: p.kp.pubkey(), tally: tally_pda(r, n), system_program: system_program::ID,
        })
    }
    fn ix_claim(&self, r: R, s: &Sealed) -> Instruction {
        Self::ix(unique::instruction::Claim {}.data(), unique::accounts::Claim {
            round: round_pda(r), entry: entry_pda(r, &s.commitment), tally: tally_pda(r, s.n),
        })
    }
    /// `winners`: token accounts, lowest number first.
    fn ix_settle(&self, r: R, winners: &[Pubkey]) -> Instruction {
        let mut ix = Self::ix(unique::instruction::Settle {}.data(), unique::accounts::Settle {
            payer: self.admin.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), next_round: round_pda((r.0, r.1 + 1)),
            mint: self.mint, vault: vault_pda(), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        });
        ix.accounts.extend(winners.iter().map(|w| AccountMeta::new(*w, false)));
        ix
    }
    fn ix_fund_pool(&self, funder: &Player, r: R, amount: u64) -> Instruction {
        Self::ix(unique::instruction::FundPool { amount }.data(), unique::accounts::FundPool {
            funder: funder.kp.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), mint: self.mint,
            funder_token: funder.ata, vault: vault_pda(), token_program: TOKEN_PROGRAM,
        })
    }
    fn ix_close_entry(&self, r: R, s: &Sealed, player: Pubkey) -> Instruction {
        Self::ix(unique::instruction::CloseEntry {}.data(), unique::accounts::CloseEntry {
            config: config_pda(), round: round_pda(r), entry: entry_pda(r, &s.commitment),
            player, team_wallet: self.team_wallet,
        })
    }
    fn ix_close_tally(&self, r: R, n: u64, payer: Pubkey) -> Instruction {
        Self::ix(unique::instruction::CloseTally {}.data(), unique::accounts::CloseTally {
            round: round_pda(r), tally: tally_pda(r, n), payer,
        })
    }

    // --- v6 bundles ------------------------------------------------------------

    fn ix_enter_bundle(&self, p: &Player, r: R, b: &SealedBundle, referrer_token: Option<Pubkey>) -> Instruction {
        Self::ix(unique::instruction::EnterBundle { commitment: b.commitment, nonce: [7; 16], sealed: vec![7; b.ns.len()], max_ticket: u64::MAX }.data(), unique::accounts::EnterBundle {
            player: p.kp.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), bundle: bundle_pda(r, &b.commitment),
            mint: self.mint, player_token: p.ata, vault: vault_pda(), reserve: reserve_pda(), stake_pool: stake_pool_pda(),
            team_token: self.team_token, referrer_token, profile: profile_pda(&p.kp.pubkey()), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_enter_bundle_from_stake(&self, p: &Player, r: R, b: &SealedBundle, referrer_token: Option<Pubkey>) -> Instruction {
        Self::ix(unique::instruction::EnterBundleFromStake { commitment: b.commitment, nonce: [7; 16], sealed: vec![7; b.ns.len()], max_ticket: u64::MAX }.data(), unique::accounts::EnterBundleFromStake {
            player: p.kp.pubkey(), config: config_pda(), table: table_pda(r.0), round: round_pda(r), bundle: bundle_pda(r, &b.commitment),
            staker: staker_pda(&p.kp.pubkey()), stake_pool: stake_pool_pda(), mint: self.mint, player_token: p.ata,
            stake_vault: stake_vault_pda(), vault: vault_pda(), reserve: reserve_pda(), team_token: self.team_token,
            referrer_token, profile: profile_pda(&p.kp.pubkey()), token_program: TOKEN_PROGRAM, system_program: system_program::ID,
        })
    }
    fn ix_repick_bundle(&self, signer: &Player, r: R, old: &SealedBundle, new: &SealedBundle) -> Instruction {
        Self::ix(unique::instruction::RepickBundle { new_commitment: new.commitment, nonce: [9; 16], sealed: vec![9; new.ns.len()] }.data(), unique::accounts::RepickBundle {
            player: signer.kp.pubkey(), round: round_pda(r),
            old_bundle: bundle_pda(r, &old.commitment), new_bundle: bundle_pda(r, &new.commitment), system_program: system_program::ID,
        })
    }
    /// Reveals `ns` (normally `b.ns`) with each number's tally passed in order.
    fn ix_reveal_bundle(&self, payer: Pubkey, player: Pubkey, r: R, b: &SealedBundle, ns: &[u64], salt: [u8; 32]) -> Instruction {
        let mut ix = Self::ix(unique::instruction::RevealBundle { numbers: ns.to_vec(), salt }.data(), unique::accounts::RevealBundle {
            payer, round: round_pda(r), bundle: bundle_pda(r, &b.commitment), player, system_program: system_program::ID,
        });
        ix.accounts.extend(ns.iter().map(|&n| AccountMeta::new(tally_pda(r, n), false)));
        ix
    }
    fn ix_claim_bundle(&self, r: R, b: &SealedBundle, index: u8, n: u64) -> Instruction {
        Self::ix(unique::instruction::ClaimBundle { index }.data(), unique::accounts::ClaimBundle {
            round: round_pda(r), bundle: bundle_pda(r, &b.commitment), tally: tally_pda(r, n),
        })
    }
    fn ix_close_bundle(&self, r: R, b: &SealedBundle, player: Pubkey) -> Instruction {
        Self::ix(unique::instruction::CloseBundle {}.data(), unique::accounts::CloseBundle {
            config: config_pda(), round: round_pda(r), bundle: bundle_pda(r, &b.commitment), player, team_wallet: self.team_wallet,
        })
    }
    fn enter_bundle(&mut self, p: &Player, r: R, b: &SealedBundle) -> Result<(), String> {
        let ix = self.ix_enter_bundle(p, r, b, None);
        self.send(&[ix], &[&p.kp])
    }
    fn reveal_bundle(&mut self, p: &Player, r: R, b: &SealedBundle) -> Result<(), String> {
        let ix = self.ix_reveal_bundle(p.kp.pubkey(), p.kp.pubkey(), r, b, &b.ns, b.salt);
        self.send(&[ix], &[&p.kp])
    }
    fn claim_bundle(&mut self, r: R, b: &SealedBundle, index: u8) -> Result<(), String> {
        let ix = self.ix_claim_bundle(r, b, index, b.ns[index as usize]);
        self.send(&[ix], &[])
    }

    // --- shorthands ------------------------------------------------------------

    fn create_table(&mut self, id: u8, p: TableParams) -> Result<(), String> {
        let ix = self.ix_create_table(self.admin.pubkey(), id, p);
        self.send(&[ix], &[])
    }
    fn set_table_enabled(&mut self, t: u8, enabled: bool) -> Result<(), String> {
        let ix = self.ix_set_table_enabled(self.admin.pubkey(), t, enabled);
        self.send(&[ix], &[])
    }
    fn enter(&mut self, p: &Player, r: R, s: &Sealed) -> Result<(), String> {
        let ix = self.ix_enter(p, r, s);
        self.send(&[ix], &[&p.kp])
    }
    fn reveal(&mut self, p: &Player, r: R, s: &Sealed) -> Result<(), String> {
        let ix = self.ix_reveal(p, r, s, s.n, s.salt);
        self.send(&[ix], &[&p.kp])
    }
    fn claim(&mut self, r: R, s: &Sealed) -> Result<(), String> {
        let ix = self.ix_claim(r, s);
        self.send(&[ix], &[])
    }
    fn settle(&mut self, r: R, winners: &[Pubkey]) -> Result<(), String> {
        let ix = self.ix_settle(r, winners);
        self.send(&[ix], &[])
    }
    fn stake(&mut self, p: &Player, amount: u64) -> Result<(), String> {
        let ix = self.ix_stake(p, amount);
        self.send(&[ix], &[&p.kp])
    }
    fn unstake(&mut self, p: &Player, amount: u64) -> Result<(), String> {
        let ix = self.ix_unstake(p, amount);
        self.send(&[ix], &[&p.kp])
    }
    fn claim_rewards(&mut self, p: &Player) -> Result<(), String> {
        let ix = self.ix_claim_rewards(p);
        self.send(&[ix], &[&p.kp])
    }
    fn enter_from_stake(&mut self, p: &Player, r: R, s: &Sealed) -> Result<(), String> {
        let ix = self.ix_enter_from_stake(p, r, s);
        self.send(&[ix], &[&p.kp])
    }
    /// n picks by a throwaway player, all different numbers.
    fn picks(&mut self, r: R, n: u64) {
        let p = self.player(100);
        for i in 0..n { let s = seal(1000 + i, &p.kp.pubkey()); self.enter(&p, r, &s).unwrap(); }
    }
}

/// Rewards per ticket: 5% of 5 tokens.
const REWARD: u64 = 5 * DEC / 20;

#[track_caller]
fn close_to(actual: u64, expected: u64, what: &str) {
    assert!(actual.abs_diff(expected) <= 2, "{what}: expected ~{expected}, got {actual}");
}

#[track_caller]
fn fails(r: Result<(), String>, expected: &str) {
    match r {
        Ok(()) => panic!("expected {expected}, but it succeeded"),
        Err(logs) => assert!(logs.contains(expected), "expected {expected}, got:\n{logs}"),
    }
}

/// Round 1 has 5 picks, which (with a 4-pick threshold for 2 winners) pays two places.
/// A and B both pick 3 (knocked out), C picks 5, E picks 9, D picks 7 and never reveals.
/// C wins 85% with 5, E gets 15% with 9.
#[test]
fn full_round_with_two_winners() {
    let mut g = Game::new([4, 100, 1000]);
    let [a, b, c, d, e, f] = g.players::<6>();
    let supply0 = g.supply();
    let each = tok(5) / 20; // 5% of a ticket

    // --- Open ---
    let sa = seal(3, &a.kp.pubkey());
    let sb = seal(3, &b.kp.pubkey());
    let sc = seal(5, &c.kp.pubkey());
    let sd = seal(7, &d.kp.pubkey());
    let se = seal(9, &e.kp.pubkey());
    assert_eq!(g.round(R1).close_at, 0, "no picks: no close time");
    g.enter(&a, R1, &sa).unwrap();
    assert_eq!(g.round(R1).close_at, CLOSE, "the first pick: closes at the next full hour");
    g.enter(&b, R1, &sb).unwrap();
    g.enter(&c, R1, &sc).unwrap();
    g.enter(&d, R1, &sd).unwrap(); // more picks before the close are fine
    g.enter(&e, R1, &se).unwrap();

    let pool = tok(5) * 85 / 100 * 5;
    let r = g.round(R1);
    assert_eq!(r.entries, 5);
    assert_eq!(r.pool_tokens, pool);
    assert_eq!(g.balance(&vault_pda()), pool, "85% of each ticket to the pool");
    assert_eq!(g.balance(&reserve_pda()), each * 5, "5% to the staking reserve");
    assert_eq!(g.balance(&g.team_token), each * 5, "5% to the team");
    assert_eq!(supply0 - g.supply(), each * 5, "5% burned");
    assert_eq!(g.balance(&a.ata), tok(95));
    assert!(g.lamports(&entry_pda(R1, &sa.commitment)) > DEPOSIT, "deposit held in the entry");
    let entry: Entry = g.fetch(&entry_pda(R1, &sa.commitment));
    assert_eq!(entry.backup, [7; BACKUP_LEN], "encrypted backup stored as given");
    assert_eq!(entry.round, round_pda(R1));

    fails(g.enter(&a, R1, &sa), "already in use");
    fails(g.reveal(&a, R1, &sa), "NotRevealTime");

    // --- Reveal ---
    g.warp_to(CLOSE);
    fails(g.enter(&f, R1, &seal(1, &f.kp.pubkey())), "EntriesClosed");

    let wrong_number = g.ix_reveal(&a, R1, &sa, 4, sa.salt);
    fails(g.send(&[wrong_number], &[&a.kp]), "CommitmentMismatch");
    let wrong_salt = g.ix_reveal(&a, R1, &sa, 3, [7; 32]);
    fails(g.send(&[wrong_salt], &[&a.kp]), "CommitmentMismatch");

    let before = g.lamports(&c.kp.pubkey());
    g.reveal(&c, R1, &sc).unwrap();
    let tally_rent = g.lamports(&tally_pda(R1, 5));
    assert_eq!(g.lamports(&c.kp.pubkey()), before + DEPOSIT - tally_rent, "deposit back, minus the new tally's rent");
    fails(g.reveal(&c, R1, &sc), "AlreadyRevealed");

    g.reveal(&a, R1, &sa).unwrap();
    g.reveal(&b, R1, &sb).unwrap();
    g.reveal(&e, R1, &se).unwrap();
    assert_eq!(g.tally(R1, 3).count, 2);
    assert_eq!(g.tally(R1, 5).count, 1);
    assert_eq!(g.round(R1).revealed, 4);

    // --- Claim ---
    fails(g.claim(R1, &sc), "NotClaimTime");
    g.warp_to(CLOSE + REVEAL);
    fails(g.reveal(&d, R1, &sd), "NotRevealTime"); // too late for D

    g.claim(R1, &se).unwrap();
    assert_eq!(g.claimed(R1), vec![9]);
    g.claim(R1, &sc).unwrap();
    assert_eq!(g.claimed(R1), vec![5, 9], "kept in order, lowest first");
    assert_eq!(g.round(R1).claims[0].player, c.kp.pubkey());
    fails(g.claim(R1, &se), "AlreadyClaimed");
    fails(g.claim(R1, &sa), "NotUnique");
    fails(g.claim(R1, &sd), "AccountNotInitialized"); // 7 was never revealed

    // --- Settle ---
    fails(g.settle(R1, &[c.ata, e.ata]), "TooEarly");
    g.warp_to(ALL_CLOSED);
    fails(g.settle(R1, &[e.ata, c.ata]), "WrongWinnerAccount");
    fails(g.settle(R1, &[c.ata]), "WinnerAccountMissing");
    g.settle(R1, &[c.ata, e.ata]).unwrap();

    let first = pool * 85 / 100;
    let second = pool * 15 / 100;
    assert_eq!(g.balance(&c.ata), tok(95) + first, "C: 85% of the pool");
    assert_eq!(g.balance(&e.ata), tok(95) + second, "E: 15% of the pool");
    assert_eq!(g.balance(&vault_pda()), pool - first - second, "only rounding dust left");
    assert!(g.round(R1).settled);
    assert_eq!(g.table(0).current_round, 2);
    let r2 = g.round(R2);
    assert_eq!(r2.pool_tokens, pool - first - second, "dust rolls over");
    assert_eq!(r2.close_at, 0, "the next round waits for its first pick");
    assert!(g.settle(R1, &[c.ata, e.ata]).is_err(), "can't settle twice");

    // --- Cleanup: rent back to revealers, unrevealed deposit to the team ---
    let before = g.lamports(&a.kp.pubkey());
    let ix = g.ix_close_entry(R1, &sa, a.kp.pubkey());
    g.send(&[ix], &[]).unwrap();
    assert!(g.lamports(&a.kp.pubkey()) > before);

    let d_entry = g.lamports(&entry_pda(R1, &sd.commitment));
    let ix = g.ix_close_entry(R1, &sd, d.kp.pubkey());
    g.send(&[ix], &[]).unwrap();
    assert_eq!(g.lamports(&g.team_wallet), d_entry, "deposit + rent forfeited");

    let before = g.lamports(&a.kp.pubkey());
    let rent = g.lamports(&tally_pda(R1, 3));
    let ix = g.ix_close_tally(R1, 3, a.kp.pubkey()); // A revealed 3 first, so A paid for it
    g.send(&[ix], &[]).unwrap();
    assert_eq!(g.lamports(&a.kp.pubkey()), before + rent);
    let wrong_payer = g.ix_close_tally(R1, 5, a.kp.pubkey()); // C paid for 5
    fails(g.send(&[wrong_payer], &[]), "ConstraintHasOne");
}

/// A round with no picks just waits, however many hours pass. The first pick, whenever it
/// comes, closes it at the next full hour; a single pick is enough.
#[test]
fn an_empty_round_waits_and_the_first_pick_starts_the_clock() {
    let mut g = Game::new([1000, 5000, 20000]);
    g.warp_to(CLOSE + 5 * HOUR + 10 * 60); // 15:10, nobody played
    assert_eq!(g.round(R1).close_at, 0);
    let a = g.player(100);
    let sa = seal(4, &a.kp.pubkey());
    g.enter(&a, R1, &sa).unwrap();
    let close = CLOSE + 6 * HOUR;
    assert_eq!(g.round(R1).close_at, close, "closes at 16:00");
    g.picks(R1, 3);
    assert_eq!(g.round(R1).close_at, close, "later picks don't move it");

    // One pick is a whole round: it reveals, wins and is paid.
    let mut g = Game::new([1000, 5000, 20000]);
    let a = g.player(100);
    let sa = seal(4, &a.kp.pubkey());
    g.enter(&a, R1, &sa).unwrap();
    g.warp_to(CLOSE);
    g.reveal(&a, R1, &sa).unwrap();
    g.claim(R1, &sa).unwrap();
    g.warp_to(CLOSE + CLAIM);
    g.settle(R1, &[a.ata]).unwrap();
    assert_eq!(g.balance(&a.ata), tok(95) + tok(5) * 85 / 100, "the lone pick wins the pot");

    // Exactly on the hour counts as the next hour: never a zero-length countdown.
    let mut g = Game::new([1000, 5000, 20000]);
    g.warp_to(CLOSE);
    g.picks(R1, 1);
    assert_eq!(g.round(R1).close_at, CLOSE + HOUR);
}

/// Tables with different offsets close at their own minute (:00, :20, :40).
#[test]
fn staggered_tables_close_at_their_own_minute() {
    let mut g = Game::new([1000, 5000, 20000]);
    g.create_table(1, hourly(20 * 60)).unwrap();
    g.create_table(2, hourly(40 * 60)).unwrap();
    fails(g.create_table(3, hourly(HOUR)), "BadConfig"); // offset must be inside the period

    g.warp_to(START + 30 * 60); // 10:30
    g.picks((0, 1), 1);
    g.picks((1, 1), 1);
    g.picks((2, 1), 1);
    assert_eq!(g.round((0, 1)).close_at, START + HOUR, "11:00");
    assert_eq!(g.round((1, 1)).close_at, START + HOUR + 20 * 60, "11:20");
    assert_eq!(g.round((2, 1)).close_at, START + 40 * 60, "10:40");

    // Rounds, picks and tallies are per table: the same commitment can be used on two tables.
    let p = g.player(100);
    let s = seal(4, &p.kp.pubkey());
    g.enter(&p, (0, 1), &s).unwrap();
    g.enter(&p, (1, 1), &s).unwrap();
    assert_eq!(g.round((1, 1)).entries, 2);
    assert_eq!(g.balance(&vault_pda()), tok(5) * 85 / 100 * 5, "one vault holds every table's pool");
}

/// Rounds opened before v2 waited for a minimum number of picks, so one can be open with picks
/// and no close time. Anyone can start its clock: it closes at its next scheduled time. Rounds
/// with no picks, or that already have a close, are left alone.
#[test]
fn a_round_from_before_v2_gets_its_close() {
    let mut g = Game::new([1000, 5000, 20000]);
    let ix = g.ix_start_close(0);
    fails(g.send(&[ix], &[]), "NothingToStart"); // no picks yet

    g.picks(R1, 3);
    let ix = g.ix_start_close(0);
    fails(g.send(&[ix], &[]), "NothingToStart"); // already closes at CLOSE

    // Turn round 1 into a v1 round still waiting for its 20 picks.
    let key = round_pda(R1);
    let mut r = g.round(R1);
    r.close_at = 0;
    r.legacy_min_picks = 20;
    let mut data = Vec::new();
    anchor_lang::AccountSerialize::try_serialize(&r, &mut data).unwrap();
    let mut acc = g.svm.get_account(&key).unwrap();
    acc.data[..data.len()].copy_from_slice(&data);
    g.svm.set_account(key, acc).unwrap();

    g.warp_to(START + 20 * 60); // 10:20
    let ix = g.ix_start_close(0);
    g.send(&[ix], &[]).unwrap(); // sent by nobody in particular (the fee payer)
    assert_eq!(g.round(R1).close_at, CLOSE, "closes at 11:00, 3 picks are enough now");
    assert_eq!(g.round(R1).entries, 3);
}

/// A table can be switched off (hidden, no new picks) only while its round is empty.
#[test]
fn tables_switch_off_only_when_empty() {
    let mut g = Game::new([1000, 5000, 20000]);
    let [a, b] = g.players::<2>();
    let ix = g.ix_set_table_enabled(a.kp.pubkey(), 0, false);
    fails(g.send(&[ix], &[&a.kp]), "ConstraintHasOne");

    let sa = seal(1, &a.kp.pubkey());
    g.enter(&a, R1, &sa).unwrap();
    fails(g.set_table_enabled(0, false), "TableNotEmpty");

    g.warp_to(CLOSE);
    g.reveal(&a, R1, &sa).unwrap(); // everyone revealed: the claim is open now
    g.claim(R1, &sa).unwrap();
    g.warp_to(CLOSE + CLAIM);
    g.settle(R1, &[a.ata]).unwrap();

    g.set_table_enabled(0, false).unwrap(); // round 2 is empty
    assert!(!g.table(0).enabled);
    fails(g.enter(&b, R2, &seal(2, &b.kp.pubkey())), "TableDisabled");
    g.set_table_enabled(0, true).unwrap();
    g.enter(&b, R2, &seal(2, &b.kp.pubkey())).unwrap();
}

/// 3 picks and a 3-winner tier (75/15/10), but only two picks get revealed.
/// The unclaimed 3rd place rolls over.
#[test]
fn unclaimed_places_roll_over() {
    let mut g = Game::new([1, 2, 1000]);
    let [a, b, c] = g.players::<3>();
    let (sa, sb, sc) = (seal(1, &a.kp.pubkey()), seal(2, &b.kp.pubkey()), seal(3, &c.kp.pubkey()));
    g.enter(&a, R1, &sa).unwrap();
    g.enter(&b, R1, &sb).unwrap();
    g.enter(&c, R1, &sc).unwrap();
    assert_eq!(g.round(R1).places(), 3);

    g.warp_to(CLOSE);
    g.reveal(&a, R1, &sa).unwrap();
    g.reveal(&c, R1, &sc).unwrap(); // B never reveals
    g.warp_to(CLOSE + REVEAL);
    g.claim(R1, &sc).unwrap();
    g.claim(R1, &sa).unwrap();
    g.warp_to(ALL_CLOSED);
    g.settle(R1, &[a.ata, c.ata]).unwrap();

    let pool = tok(5) * 85 / 100 * 3;
    assert_eq!(g.balance(&a.ata), tok(95) + pool * 75 / 100, "1st place: 75%");
    assert_eq!(g.balance(&c.ata), tok(95) + pool * 15 / 100, "2nd place: 15%");
    assert_eq!(g.round(R2).pool_tokens, pool - pool * 75 / 100 - pool * 15 / 100, "3rd place's 10% rolls over");
}

/// With 4 places, a 5th unique number can only get in by being lower than the current 4th.
#[test]
fn keeps_only_the_four_lowest() {
    let mut g = Game::new([1, 1, 1]);
    let players = g.players::<5>();
    let sealed: Vec<Sealed> = players.iter().zip([10, 20, 30, 40, 50]).map(|(p, n)| seal(n, &p.kp.pubkey())).collect();
    for (p, s) in players.iter().zip(&sealed) { g.enter(p, R1, s).unwrap(); }
    g.warp_to(CLOSE);
    for (p, s) in players.iter().zip(&sealed) { g.reveal(p, R1, s).unwrap(); }
    // Everyone revealed, so the claim window is already open.

    for i in [4, 3, 2, 1] { g.claim(R1, &sealed[i]).unwrap(); } // 50, 40, 30, 20
    assert_eq!(g.claimed(R1), vec![20, 30, 40, 50]);
    g.claim(R1, &sealed[0]).unwrap(); // 10 pushes 50 out
    assert_eq!(g.claimed(R1), vec![10, 20, 30, 40]);
    fails(g.claim(R1, &sealed[4]), "NotLowest");

    g.warp_to(ALL_CLOSED);
    let atas: Vec<Pubkey> = players[..4].iter().map(|p| p.ata).collect();
    g.settle(R1, &atas).unwrap();
    let pool = tok(5) * 85 / 100 * 5;
    for (p, share) in players[..4].iter().zip([70, 15, 10, 5]) {
        assert_eq!(g.balance(&p.ata), tok(95) + pool * share / 100);
    }
    assert_eq!(g.balance(&players[4].ata), tok(95), "5th unique number gets nothing");
}

/// When every entry is revealed, the reveal window ends there and then: claims open at once
/// and the round settles one claim window later, not after the full reveal time.
#[test]
fn everyone_revealed_ends_the_reveal_early() {
    let mut g = Game::new([1000, 5000, 20000]);
    let [a, b] = g.players::<2>();
    let (sa, sb) = (seal(4, &a.kp.pubkey()), seal(9, &b.kp.pubkey()));
    g.enter(&a, R1, &sa).unwrap();
    g.enter(&b, R1, &sb).unwrap();

    g.warp_to(CLOSE + 5);
    g.reveal(&a, R1, &sa).unwrap();
    fails(g.claim(R1, &sa), "NotClaimTime"); // B hasn't revealed yet
    g.reveal(&b, R1, &sb).unwrap();
    assert_eq!(g.round(R1).all_revealed_at, CLOSE + 5);

    g.claim(R1, &sa).unwrap(); // claim opens immediately, not at CLOSE + REVEAL
    fails(g.settle(R1, &[a.ata]), "TooEarly");
    g.warp_to(CLOSE + 5 + CLAIM);
    g.settle(R1, &[a.ata]).unwrap();
    assert_eq!(g.balance(&a.ata), tok(95) + tok(5) * 85 / 100 * 2);
}

#[test]
fn nobody_reveals_so_the_pool_rolls_over() {
    let mut g = Game::new([1000, 5000, 20000]);
    let funder = g.player(100);
    let f = g.player(100);

    let ix = g.ix_fund_pool(&funder, R1, tok(20));
    g.send(&[ix], &[&funder.kp]).unwrap();
    assert_eq!(g.round(R1).close_at, 0, "funding alone doesn't count as a pick");
    g.enter(&f, R1, &seal(1, &f.kp.pubkey())).unwrap();
    assert!(g.round(R1).close_at > 0);

    g.warp_to(ALL_CLOSED);
    g.settle(R1, &[]).unwrap();

    let pool = tok(20) + tok(5) * 85 / 100;
    assert_eq!(g.round(R2).pool_tokens, pool, "20 funded + 4.25 from the entry roll over");
    assert_eq!(g.balance(&vault_pda()), pool);
}

#[test]
fn only_the_admin_changes_rules_and_only_for_future_rounds() {
    let tiers = [1000, 5000, 20000];
    let mut g = Game::new(tiers);
    let intruder = g.player(100);

    let mut p = hourly(0);
    p.ticket_tokens = tok(1);
    let ix = g.ix_update_table(intruder.kp.pubkey(), 0, p);
    fails(g.send(&[ix], &[&intruder.kp]), "ConstraintHasOne");
    let ix = g.ix_create_table(intruder.kp.pubkey(), 9, hourly(0));
    fails(g.send(&[ix], &[&intruder.kp]), "ConstraintHasOne");
    let ix = g.ix_update_config(intruder.kp.pubkey(), g.params(tiers));
    fails(g.send(&[ix], &[&intruder.kp]), "ConstraintHasOne");

    let mut p = hourly(0);
    p.ticket_tokens = tok(7);
    let ix = g.ix_update_table(g.admin.pubkey(), 0, p);
    g.send(&[ix], &[]).unwrap();
    assert_eq!(g.table(0).ticket_tokens, tok(7));
    assert_eq!(g.round(R1).ticket_tokens, tok(5), "the running round keeps its price");

    let player = g.player(100);
    g.enter(&player, R1, &seal(2, &player.kp.pubkey())).unwrap();
    assert_eq!(g.balance(&player.ata), tok(95));

    let mut bad = g.params(tiers);
    bad.pool_bps = 9500; // 95% + 5% + 5% > 100%
    let ix = g.ix_update_config(g.admin.pubkey(), bad);
    fails(g.send(&[ix], &[]), "BadConfig");

    let mut bad = g.params(tiers);
    bad.tier_splits[1] = [8000, 1500, 0, 0]; // doesn't add up to 100%
    let ix = g.ix_update_config(g.admin.pubkey(), bad);
    fails(g.send(&[ix], &[]), "BadConfig");

    let ix = g.ix_update_table(g.admin.pubkey(), 0, hourly(HOUR)); // the close must be inside the hour
    fails(g.send(&[ix], &[]), "BadConfig");

    // Guardrails: the admin can't shrink the pool below 70%, give marketing more than 10%,
    // let the keeper swing prices more than ±50%, or make the windows too short to use.
    let mut bad = g.params(tiers);
    bad.pool_bps = 6000; bad.burn_bps = 3400;
    let ix = g.ix_update_config(g.admin.pubkey(), bad);
    fails(g.send(&[ix], &[]), "BadConfig");
    let mut bad = g.params(tiers);
    bad.pool_bps = 8000; bad.burn_bps = 0; bad.referral_bps = 0; // marketing would get 15%
    let ix = g.ix_update_config(g.admin.pubkey(), bad);
    fails(g.send(&[ix], &[]), "BadConfig");
    let mut bad = g.params(tiers);
    bad.max_price_change_bps = 9000;
    let ix = g.ix_update_config(g.admin.pubkey(), bad);
    fails(g.send(&[ix], &[]), "BadConfig");
    let mut bad = hourly(0);
    bad.reveal_secs = 5;
    let ix = g.ix_update_table(g.admin.pubkey(), 0, bad);
    fails(g.send(&[ix], &[]), "BadConfig");
}

// ---------------------------------------------------------------------------
// Staking
// ---------------------------------------------------------------------------

const TIERS: [u64; 3] = [1000, 5000, 20000];

/// Every pick's rewards share goes to stakers in proportion to their stake,
/// counted from the moment they stake.
#[test]
fn staking_rewards_are_shared_pro_rata() {
    let mut g = Game::new(TIERS);
    let [a, b, c] = g.players::<3>();
    g.stake(&a, tok(10)).unwrap();
    g.stake(&b, tok(30)).unwrap();
    assert_eq!(g.balance(&a.ata), tok(90));
    assert_eq!(g.balance(&stake_vault_pda()), tok(40));
    assert_eq!(g.stake_pool().total_staked, tok(40));

    g.picks(R1, 4); // 4 picks × 0.25 = 1 token of rewards
    let (a0, b0) = (g.balance(&a.ata), g.balance(&b.ata));
    g.claim_rewards(&a).unwrap();
    g.claim_rewards(&b).unwrap();
    close_to(g.balance(&a.ata) - a0, REWARD, "A holds 1/4 of the stake");
    close_to(g.balance(&b.ata) - b0, 3 * REWARD, "B holds 3/4");
    fails(g.claim_rewards(&a), "NothingToClaim");

    // Joining later earns only from then on. Picks on any table feed the same pool.
    g.stake(&c, tok(40)).unwrap(); // now A 10, B 30, C 40
    let mut hi = hourly(0);
    hi.ticket_tokens = tok(50);
    g.create_table(2, hi).unwrap();
    g.picks((2, 1), 1); // one $50 pick = 2.5 tokens of rewards
    let c0 = g.balance(&c.ata);
    g.claim_rewards(&c).unwrap();
    close_to(g.balance(&c.ata) - c0, 5 * REWARD, "C holds half of the stake now");
}

/// Rewards that arrive while nobody is staked are released over 30 days of staked time.
#[test]
fn rewards_with_nobody_staked_release_over_30_days() {
    let mut g = Game::new(TIERS);
    g.picks(R1, 4);
    assert_eq!(g.stake_pool().backlog, 4 * REWARD, "nobody staked: it waits");

    let a = g.player(100);
    g.warp_to(START + 10 * DAY); // still nobody staked: the 30 days haven't started
    g.stake(&a, tok(10)).unwrap();
    g.warp_to(START + 25 * DAY); // 15 staked days: half
    let a0 = g.balance(&a.ata);
    g.claim_rewards(&a).unwrap();
    close_to(g.balance(&a.ata) - a0, 2 * REWARD, "half after 15 of 30 days");

    g.unstake(&a, tok(10)).unwrap();
    g.warp_to(START + 35 * DAY); // 10 days with nobody staked: the release pauses
    g.stake(&a, tok(10)).unwrap();
    close_to(g.stake_pool().backlog, 2 * REWARD, "nothing released while nobody was staked");
    g.warp_to(START + 50 * DAY); // the remaining 15 staked days
    let a1 = g.balance(&a.ata);
    g.claim_rewards(&a).unwrap();
    close_to(g.balance(&a.ata) - a1, 2 * REWARD, "the rest after 30 staked days in total");
    assert_eq!(g.stake_pool().backlog, 0);
    g.warp_to(START + 60 * DAY);
    fails(g.claim_rewards(&a), "NothingToClaim");
}

#[test]
fn unstake_any_time_with_a_minimum_stake() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    fails(g.stake(&b, 0), "BadAmount");
    fails(g.stake(&b, tok(1) / 2), "BelowMinimumStake");

    g.stake(&a, tok(10)).unwrap();
    g.picks(R1, 4);
    fails(g.unstake(&a, tok(11)), "NotEnoughStaked");
    fails(g.unstake(&a, tok(10) - tok(1) / 2), "BelowMinimumStake"); // would leave 0.5
    g.unstake(&a, tok(9)).unwrap();
    assert_eq!(g.balance(&a.ata), tok(99));
    assert_eq!(g.staker(&a).amount, tok(1));
    g.unstake(&a, tok(1)).unwrap(); // instantly, no waiting period
    assert_eq!(g.balance(&a.ata), tok(100));
    assert_eq!(g.stake_pool().total_staked, 0);

    let a0 = g.balance(&a.ata);
    g.claim_rewards(&a).unwrap(); // rewards earned while staked stay claimable
    close_to(g.balance(&a.ata) - a0, 4 * REWARD, "all 4 picks' rewards");
}

/// Picks paid from staking: unclaimed rewards first, then the stake. A leftover stake below
/// the minimum goes back to the wallet.
#[test]
fn picks_paid_from_rewards_then_stake() {
    let mut g = Game::new(TIERS);
    let mut p = g.params(TIERS);
    p.min_stake_tokens = tok(2);
    let ix = g.ix_update_config(g.admin.pubkey(), p);
    g.send(&[ix], &[]).unwrap();

    let a = g.player(100);
    g.stake(&a, tok(10)).unwrap();
    g.picks(R1, 4); // A is the only staker: 1 token of rewards
    let (vault0, team0, supply0) = (g.balance(&vault_pda()), g.balance(&g.team_token), g.supply());

    g.enter_from_stake(&a, R1, &seal(7, &a.kp.pubkey())).unwrap();
    assert_eq!(g.staker(&a).amount, tok(6), "1 token from rewards, 4 from the stake");
    assert_eq!(g.balance(&a.ata), tok(90), "wallet untouched");
    assert_eq!(g.balance(&vault_pda()) - vault0, tok(5) * 85 / 100, "85% to the pool");
    assert_eq!(g.balance(&g.team_token) - team0, REWARD, "5% to the team");
    assert_eq!(supply0 - g.supply(), REWARD, "5% burned");
    assert_eq!(g.round(R1).entries, 5);

    // Second pick: 0.25 (A's rewards from its own last pick) + 4.75 from the stake leaves
    // 1.25 staked, under the 2 token minimum, so it goes back to the wallet.
    g.enter_from_stake(&a, R1, &seal(8, &a.kp.pubkey())).unwrap();
    assert_eq!(g.staker(&a).amount, 0);
    close_to(g.balance(&a.ata), tok(90) + tok(5) / 4, "leftover 1.25 returned");
    assert_eq!(g.stake_pool().total_staked, 0);
    close_to(g.stake_pool().backlog, REWARD, "this pick's rewards wait for the next staker");
    fails(g.enter_from_stake(&a, R1, &seal(9, &a.kp.pubkey())), "NotEnoughStaked");
}

/// The keeper can only nudge a table's price: at most ±25%, at most every 10 minutes. The
/// change applies to the table's open round straight away, and to every round after.
#[test]
fn keeper_adjusts_the_price_within_limits() {
    let mut g = Game::new(TIERS);
    g.create_table(1, hourly(0)).unwrap();
    let intruder = g.player(100);
    let keeper = g.keeper.insecure_clone();

    let ix = g.ix_set_ticket(intruder.kp.pubkey(), 0, tok(6));
    fails(g.send(&[ix], &[&intruder.kp]), "Unauthorized");
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(7)); // +40%
    fails(g.send(&[ix], &[&keeper]), "PriceChangeTooBig");
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, 0);
    fails(g.send(&[ix], &[&keeper]), "PriceChangeTooBig");

    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(6)); // +20%
    g.send(&[ix], &[&keeper]).unwrap();
    assert_eq!(g.table(0).ticket_tokens, tok(6));
    assert_eq!(g.round(R1).ticket_tokens, tok(6), "the open round follows the price");

    // Not again within 10 minutes; other tables have their own clock.
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(5));
    fails(g.send(&[ix], &[&keeper]), "PriceAlreadySet");
    g.warp_to(START + 599);
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(5));
    fails(g.send(&[ix], &[&keeper]), "PriceAlreadySet");
    let ix = g.ix_set_ticket(keeper.pubkey(), 1, tok(4));
    g.send(&[ix], &[&keeper]).unwrap();
    g.warp_to(START + 600);
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(5)); // -16.7%
    g.send(&[ix], &[&keeper]).unwrap();
    assert_eq!(g.round(R1).ticket_tokens, tok(5));

    // A pick pays the price of the moment.
    let p = g.player(100);
    let s = seal(1, &p.kp.pubkey());
    let before = g.balance(&p.ata);
    g.enter(&p, R1, &s).unwrap();
    assert_eq!(before - g.balance(&p.ata), tok(5));

    // Once the round has closed, a change only applies to the rounds after it.
    g.warp_to(CLOSE);
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(6));
    g.send(&[ix], &[&keeper]).unwrap();
    assert_eq!(g.table(0).ticket_tokens, tok(6));
    assert_eq!(g.round(R1).ticket_tokens, tok(5), "a closed round keeps its price");
    g.reveal(&p, R1, &s).unwrap();
    g.warp_to(CLOSE + CLAIM);
    g.settle(R1, &[]).unwrap(); // nobody claimed: rolls over
    assert_eq!(g.round(R2).ticket_tokens, tok(6));
}

/// A pick names the most it will pay. If the keeper raised the price in the meantime, the pick
/// is refused rather than charged more; a lower price goes through.
#[test]
fn a_pick_never_pays_more_than_the_player_saw() {
    let mut g = Game::new(TIERS);
    let keeper = g.keeper.insecure_clone();
    let [a, b] = g.players::<2>();
    g.stake(&b, tok(20)).unwrap();

    let ix = g.ix_enter_max(&a, R1, &seal(1, &a.kp.pubkey()), None, tok(5));
    g.send(&[ix], &[&a.kp]).unwrap(); // exactly the price: fine

    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(6));
    g.send(&[ix], &[&keeper]).unwrap();
    let ix = g.ix_enter_max(&a, R1, &seal(2, &a.kp.pubkey()), None, tok(5));
    fails(g.send(&[ix], &[&a.kp]), "TicketPriceChanged");
    let ix = g.ix_enter_from_stake_max(&b, R1, &seal(3, &b.kp.pubkey()), None, tok(5));
    fails(g.send(&[ix], &[&b.kp]), "TicketPriceChanged");
    let ix = g.ix_enter_max(&a, R1, &seal(2, &a.kp.pubkey()), None, tok(6));
    g.send(&[ix], &[&a.kp]).unwrap(); // after seeing the new price: fine

    g.warp_to(START + 600);
    let ix = g.ix_set_ticket(keeper.pubkey(), 0, tok(5));
    g.send(&[ix], &[&keeper]).unwrap();
    let before = g.balance(&a.ata);
    let ix = g.ix_enter_max(&a, R1, &seal(4, &a.kp.pubkey()), None, tok(6));
    g.send(&[ix], &[&a.kp]).unwrap(); // the price fell: pays the lower one
    assert_eq!(before - g.balance(&a.ata), tok(5));
    assert_eq!(g.round(R1).entries, 3);
}

/// Pausing stops new picks and nothing else.
#[test]
fn pause_stops_only_new_picks() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    g.stake(&a, tok(10)).unwrap();

    let ix = g.ix_set_paused(b.kp.pubkey(), true);
    fails(g.send(&[ix], &[&b.kp]), "ConstraintHasOne");
    let ix = g.ix_set_paused(g.admin.pubkey(), true);
    g.send(&[ix], &[]).unwrap();

    fails(g.enter(&b, R1, &seal(3, &b.kp.pubkey())), "EntriesPaused");
    fails(g.enter_from_stake(&a, R1, &seal(4, &a.kp.pubkey())), "EntriesPaused");
    g.unstake(&a, tok(5)).unwrap(); // staking keeps working
    g.stake(&a, tok(1)).unwrap();

    let ix = g.ix_set_paused(g.admin.pubkey(), false);
    g.send(&[ix], &[]).unwrap();
    g.enter(&b, R1, &seal(3, &b.kp.pubkey())).unwrap();
}

// ---------------------------------------------------------------------------
// Referrals and changing your pick
// ---------------------------------------------------------------------------

/// A referred pick pays 1% to the referrer, out of the team's 5%. Pool, stakers and burn are untouched.
#[test]
fn referrals_pay_one_percent_from_the_team_share() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    let one_pct = tok(5) / 100;

    let (vault0, team0, reserve0, supply0) = (g.balance(&vault_pda()), g.balance(&g.team_token), g.balance(&reserve_pda()), g.supply());
    let ix = g.ix_enter_ref(&a, R1, &seal(3, &a.kp.pubkey()), Some(b.ata));
    g.send(&[ix], &[&a.kp]).unwrap();
    assert_eq!(g.balance(&b.ata), tok(100) + one_pct, "referrer gets 1%");
    assert_eq!(g.balance(&g.team_token) - team0, REWARD - one_pct, "team gets 4% instead of 5%");
    assert_eq!(g.balance(&vault_pda()) - vault0, tok(5) * 85 / 100, "pool unchanged");
    assert_eq!(g.balance(&reserve_pda()) - reserve0, REWARD, "stakers unchanged");
    assert_eq!(supply0 - g.supply(), REWARD, "burn unchanged");

    // Unreferred picks: the team keeps its full 5%.
    let team1 = g.balance(&g.team_token);
    g.enter(&a, R1, &seal(4, &a.kp.pubkey())).unwrap();
    assert_eq!(g.balance(&g.team_token) - team1, REWARD);

    // You can't refer yourself: not with the same token account (rejected as a duplicate account)...
    let ix = g.ix_enter_ref(&a, R1, &seal(5, &a.kp.pubkey()), Some(a.ata));
    assert!(g.send(&[ix], &[&a.kp]).is_err());
    // ...and not with a second token account of your own.
    let a_second = g.token_account(a.kp.pubkey(), 0);
    let ix = g.ix_enter_ref(&a, R1, &seal(5, &a.kp.pubkey()), Some(a_second));
    fails(g.send(&[ix], &[&a.kp]), "SelfReferral");

    // Picks paid from stake pay the referrer too.
    g.stake(&a, tok(20)).unwrap();
    let b0 = g.balance(&b.ata);
    let ix = g.ix_enter_from_stake_ref(&a, R1, &seal(6, &a.kp.pubkey()), Some(b.ata));
    g.send(&[ix], &[&a.kp]).unwrap();
    assert_eq!(g.balance(&b.ata) - b0, one_pct);
}

/// A pick can be swapped for a new number until a minute before the close.
#[test]
fn change_your_pick_until_a_minute_before_close() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    let first = seal(3, &a.kp.pubkey());
    g.enter(&a, R1, &first).unwrap();
    let (entries, pool) = (g.round(R1).entries, g.round(R1).pool_tokens);
    let a0 = g.lamports(&a.kp.pubkey());

    let second = seal(7, &a.kp.pubkey());
    let ix = g.ix_repick(&b, R1, &first, &second);
    fails(g.send(&[ix], &[&b.kp]), "ConstraintHasOne"); // only your own picks
    let ix = g.ix_repick(&a, R1, &first, &second);
    g.send(&[ix], &[&a.kp]).unwrap();
    assert!(g.svm.get_account(&entry_pda(R1, &first.commitment)).map_or(true, |x| x.lamports == 0), "old entry closed");
    assert_eq!(g.round(R1).entries, entries, "still one pick");
    assert_eq!(g.round(R1).pool_tokens, pool, "no second ticket");
    assert_eq!(g.lamports(&a.kp.pubkey()), a0, "rent and deposit recycled, nothing lost");

    let third = seal(9, &a.kp.pubkey());
    g.warp_to(CLOSE - REPICK_CUTOFF - 1);
    let ix = g.ix_repick(&a, R1, &second, &third);
    g.send(&[ix], &[&a.kp]).unwrap(); // 61 s left: fine
    let fourth = seal(11, &a.kp.pubkey());
    g.warp_to(CLOSE - REPICK_CUTOFF + 1);
    let ix = g.ix_repick(&a, R1, &third, &fourth);
    fails(g.send(&[ix], &[&a.kp]), "RepickClosed"); // 59 s left: locked

    g.warp_to(CLOSE);
    fails(g.reveal(&a, R1, &first), "AccountNotInitialized"); // the old number is gone
    g.reveal(&a, R1, &third).unwrap();
    assert_eq!(g.tally(R1, 9).count, 1, "the latest number counts");
}

/// A pick on one table can't be moved to, revealed in, or claimed against another table's round.
#[test]
fn picks_stay_on_their_table() {
    let mut g = Game::new([1000, 5000, 20000]);
    g.create_table(1, hourly(0)).unwrap();
    let a = g.player(100);
    let s = seal(4, &a.kp.pubkey());
    g.enter(&a, (0, 1), &s).unwrap();
    g.picks((1, 1), 1);
    g.warp_to(CLOSE);
    // Reveal with table 0's entry but table 1's round: the entry address won't match.
    let mut ix = g.ix_reveal(&a, (1, 1), &s, s.n, s.salt);
    ix.accounts[2].pubkey = entry_pda((0, 1), &s.commitment);
    fails(g.send(&[ix], &[&a.kp]), "ConstraintSeeds");
}

/// The referrer belongs to the wallet, not the browser: once set by the first referred pick it
/// can't be swapped, and it's the only one that can be paid, whatever device the player uses.
#[test]
fn the_referrer_sticks_to_the_wallet() {
    let mut g = Game::new(TIERS);
    let [a, b, c, d] = g.players::<4>();
    let one_pct = tok(5) / 100;

    // D plays without a link: no profile, no rent.
    g.enter(&d, R1, &seal(1, &d.kp.pubkey())).unwrap();
    assert!(g.svm.get_account(&profile_pda(&d.kp.pubkey())).map_or(true, |x| x.lamports == 0), "no profile without a referrer");

    // A's first referred pick records B.
    let ix = g.ix_enter_ref(&a, R1, &seal(2, &a.kp.pubkey()), Some(b.ata));
    g.send(&[ix], &[&a.kp]).unwrap();
    let p: PlayerProfile = g.fetch(&profile_pda(&a.kp.pubkey()));
    assert_eq!(p.referrer, b.kp.pubkey());

    // A different link later can't take A over, on either payment path.
    let ix = g.ix_enter_ref(&a, R1, &seal(3, &a.kp.pubkey()), Some(c.ata));
    fails(g.send(&[ix], &[&a.kp]), "WrongReferrer");
    g.stake(&a, tok(20)).unwrap();
    let ix = g.ix_enter_from_stake_ref(&a, R1, &seal(3, &a.kp.pubkey()), Some(c.ata));
    fails(g.send(&[ix], &[&a.kp]), "WrongReferrer");

    // B keeps earning from A's picks (the site passes B's account from A's profile).
    let b0 = g.balance(&b.ata);
    let ix = g.ix_enter_ref(&a, R1, &seal(4, &a.kp.pubkey()), Some(b.ata));
    g.send(&[ix], &[&a.kp]).unwrap();
    assert_eq!(g.balance(&b.ata) - b0, one_pct);

    // A wallet that played before it had a referrer can still be referred once.
    let ix = g.ix_enter_ref(&d, R1, &seal(5, &d.kp.pubkey()), Some(c.ata));
    g.send(&[ix], &[&d.kp]).unwrap();
    let p: PlayerProfile = g.fetch(&profile_pda(&d.kp.pubkey()));
    assert_eq!(p.referrer, c.kp.pubkey());

    // Someone pre-funding the profile address doesn't block it.
    let e = g.player(100);
    g.svm.airdrop(&profile_pda(&e.kp.pubkey()), 1_000_000).unwrap();
    let ix = g.ix_enter_ref(&e, R1, &seal(6, &e.kp.pubkey()), Some(b.ata));
    g.send(&[ix], &[&e.kp]).unwrap();
    let p: PlayerProfile = g.fetch(&profile_pda(&e.kp.pubkey()));
    assert_eq!(p.referrer, b.kp.pubkey());
}

// --- v6: bundles ---------------------------------------------------------------------

/// Bundles and single picks in one round. A bundles 3, 5, 5, 12 (four tickets, one deposit);
/// B bundles just 3; C picks 7 the old way; D bundles 9, 10 and never reveals. 3 and 5 are
/// knocked out (5 twice inside A's own bundle), so 7 (C) and 12 (A) win.
#[test]
fn bundles_play_alongside_single_picks() {
    let mut g = Game::new([4, 100, 1000]);
    let [a, b, c, d] = g.players::<4>();
    let sa = seal_bundle(&[3, 5, 5, 12], &a.kp.pubkey());
    let sb = seal_bundle(&[3], &b.kp.pubkey());
    let sc = seal(7, &c.kp.pubkey());
    let sd = seal_bundle(&[9, 10], &d.kp.pubkey());
    let a_sol = g.lamports(&a.kp.pubkey());
    g.enter_bundle(&a, R1, &sa).unwrap();
    assert_eq!(g.round(R1).close_at, CLOSE, "a bundle starts the clock like a pick");
    g.enter_bundle(&b, R1, &sb).unwrap();
    g.enter(&c, R1, &sc).unwrap();
    g.enter_bundle(&d, R1, &sd).unwrap();

    let r = g.round(R1);
    assert_eq!(r.entries, 8, "every number counts as a pick");
    assert_eq!(r.pool_tokens, tok(5) * 85 / 100 * 8);
    assert_eq!(g.balance(&a.ata), tok(80), "A paid four tickets");
    let bundle = g.lamports(&bundle_pda(R1, &sa.commitment));
    let rent = g.svm.minimum_balance_for_rent_exemption(Bundle::space(4));
    assert_eq!(bundle, rent + DEPOSIT, "one account and one deposit for four numbers");
    assert_eq!(a_sol - g.lamports(&a.kp.pubkey()), rent + DEPOSIT);
    let stored: Bundle = g.fetch(&bundle_pda(R1, &sa.commitment));
    assert_eq!((stored.round, stored.player, stored.numbers.clone(), stored.revealed), (round_pda(R1), a.kp.pubkey(), vec![7; 4], false));
    fails(g.reveal_bundle(&a, R1, &sa), "NotRevealTime");

    // --- Reveal ---
    g.warp_to(CLOSE);
    let shuffled = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &sa, &[5, 3, 5, 12], sa.salt);
    fails(g.send(&[shuffled], &[&a.kp]), "CommitmentMismatch");
    let short = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &sa, &[3, 5, 5], sa.salt);
    fails(g.send(&[short], &[&a.kp]), "BadBundle");
    let mut wrong_tally = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &sa, &sa.ns, sa.salt);
    let last = wrong_tally.accounts.len() - 1;
    wrong_tally.accounts[last].pubkey = tally_pda(R1, 13);
    fails(g.send(&[wrong_tally], &[&a.kp]), "WrongTally");
    let mut missing = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &sa, &sa.ns, sa.salt);
    missing.accounts.pop();
    fails(g.send(&[missing], &[&a.kp]), "WrongTally");
    let mut not_writable = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &sa, &sa.ns, sa.salt);
    not_writable.accounts[last].is_writable = false;
    fails(g.send(&[not_writable], &[&a.kp]), "WrongTally");

    // The keeper reveals A's bundle (it pays the new tallies); A gets the deposit back.
    let (a0, k0) = (g.lamports(&a.kp.pubkey()), g.lamports(&g.keeper.pubkey()));
    let ix = g.ix_reveal_bundle(g.keeper.pubkey(), a.kp.pubkey(), R1, &sa, &sa.ns, sa.salt);
    let keeper = g.keeper.insecure_clone();
    g.send(&[ix], &[&keeper]).unwrap();
    assert_eq!(g.lamports(&a.kp.pubkey()), a0 + DEPOSIT, "deposit back to A");
    let tally_rent = g.lamports(&tally_pda(R1, 3));
    assert_eq!(k0 - g.lamports(&g.keeper.pubkey()), 3 * tally_rent, "three new tallies (5 twice is one tally)");
    assert_eq!((g.tally(R1, 3).count, g.tally(R1, 5).count, g.tally(R1, 12).count), (1, 2, 1));
    assert_eq!(g.tally(R1, 5).payer, g.keeper.pubkey());
    let stored: Bundle = g.fetch(&bundle_pda(R1, &sa.commitment));
    assert_eq!((stored.numbers.clone(), stored.revealed, stored.deposit), (vec![3, 5, 5, 12], true, 0), "the numbers, in the clear");
    fails(g.reveal_bundle(&a, R1, &sa), "AlreadyRevealed");

    g.reveal_bundle(&b, R1, &sb).unwrap();
    g.reveal(&c, R1, &sc).unwrap();
    assert_eq!(g.tally(R1, 3).count, 2, "a bundle and a bundle of one share the tally");
    assert_eq!(g.round(R1).revealed, 6);

    // --- Claim ---
    g.warp_to(CLOSE + REVEAL);
    fails(g.reveal_bundle(&d, R1, &sd), "NotRevealTime");
    fails(g.claim_bundle(R1, &sa, 0), "NotUnique"); // 3: A and B
    fails(g.claim_bundle(R1, &sa, 1), "NotUnique"); // 5: twice in A's bundle
    let out_of_range = g.ix_claim_bundle(R1, &sa, 4, 12);
    fails(g.send(&[out_of_range], &[]), "BadBundle");
    let wrong_tally = g.ix_claim_bundle(R1, &sa, 3, 7);
    fails(g.send(&[wrong_tally], &[]), "WrongTally"); // 12 with 7's tally
    g.claim_bundle(R1, &sa, 3).unwrap();
    g.claim(R1, &sc).unwrap();
    fails(g.claim_bundle(R1, &sa, 3), "AlreadyClaimed");
    assert_eq!(g.claimed(R1), vec![7, 12]);
    assert_eq!(g.round(R1).claims[1].player, a.kp.pubkey());

    // --- Settle & cleanup ---
    g.warp_to(ALL_CLOSED);
    g.settle(R1, &[c.ata, a.ata]).unwrap();
    let pool = tok(5) * 85 / 100 * 8;
    assert_eq!(g.balance(&a.ata), tok(80) + pool * 15 / 100, "A: second place, from a bundle");

    let before = g.lamports(&a.kp.pubkey());
    let ix = g.ix_close_bundle(R1, &sa, a.kp.pubkey());
    g.send(&[ix], &[]).unwrap();
    assert_eq!(g.lamports(&a.kp.pubkey()), before + rent, "A's bundle rent back");
    let d_bundle = g.lamports(&bundle_pda(R1, &sd.commitment));
    let ix = g.ix_close_bundle(R1, &sd, d.kp.pubkey());
    g.send(&[ix], &[]).unwrap();
    assert_eq!(g.lamports(&g.team_wallet), d_bundle, "D never revealed: deposit + rent forfeited");
    let ix = g.ix_close_tally(R1, 5, g.keeper.pubkey());
    g.send(&[ix], &[]).unwrap(); // tallies the keeper paid for go back to the keeper
}

/// 1 to 10 numbers per bundle. Ten fit in one transaction with the timelocked copy (a ~330-byte
/// memo) and a referrer, paid by the player, with room to spare.
#[test]
fn a_bundle_holds_one_to_ten_numbers() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    let none = seal_bundle(&[], &a.kp.pubkey());
    fails(g.enter_bundle(&a, R1, &none), "BadBundle");
    let eleven: Vec<u64> = (1..=11).collect();
    fails(g.enter_bundle(&a, R1, &seal_bundle(&eleven, &a.kp.pubkey())), "BadBundle");

    let ten: Vec<u64> = (1..=10).collect();
    let s = seal_bundle(&ten, &a.kp.pubkey());
    let enter = g.ix_enter_bundle(&a, R1, &s, Some(b.ata));
    let memo = Instruction::new_with_bytes(Pubkey::new_unique(), &[b'x'; 330], vec![]);
    let msg = Message::new_with_blockhash(&[enter.clone(), memo], Some(&a.kp.pubkey()), &g.svm.latest_blockhash());
    let size = 1 + 64 + VersionedMessage::Legacy(msg).serialize().len();
    println!("a 10-number pick with its memo and a referrer: {size} bytes of 1232");
    assert!(size <= 1232 - 40, "10 numbers must fit with room to spare, got {size}");
    let cu = g.send_cu(&[enter], &[&a.kp]);
    println!("a 10-number pick: {cu} compute units");
    assert_eq!(g.round(R1).entries, 10);
    assert_eq!(g.balance(&a.ata), tok(50));

    // Revealing ten numbers, all new tallies, fits the default compute budget.
    g.warp_to(CLOSE);
    let ix = g.ix_reveal_bundle(a.kp.pubkey(), a.kp.pubkey(), R1, &s, &s.ns, s.salt);
    let msg = Message::new_with_blockhash(&[ix.clone()], Some(&a.kp.pubkey()), &g.svm.latest_blockhash());
    let size = 1 + 64 + VersionedMessage::Legacy(msg).serialize().len();
    println!("revealing 10 numbers: {size} bytes");
    assert!(size <= 1232);
    let cu = g.send_cu(&[ix], &[&a.kp]);
    println!("revealing 10 numbers (10 new tallies): {cu} compute units");
    assert!(cu < 180_000, "inside the default 200k");
    assert_eq!(g.round(R1).revealed, 10);
    assert!(g.round(R1).all_revealed_at > 0, "everyone revealed: the reveal ends early");
}

/// Changing one number re-seals its bundle: same size, tickets carry over, nothing lost.
#[test]
fn change_a_number_in_a_bundle() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    let first = seal_bundle(&[3, 5, 8], &a.kp.pubkey());
    g.enter_bundle(&a, R1, &first).unwrap();
    let (entries, pool, a0) = (g.round(R1).entries, g.round(R1).pool_tokens, g.lamports(&a.kp.pubkey()));

    let second = seal_bundle(&[3, 6, 8], &a.kp.pubkey());
    let ix = g.ix_repick_bundle(&b, R1, &first, &second);
    fails(g.send(&[ix], &[&b.kp]), "ConstraintHasOne"); // only your own
    let smaller = seal_bundle(&[3, 6], &a.kp.pubkey());
    let ix = g.ix_repick_bundle(&a, R1, &first, &smaller);
    fails(g.send(&[ix], &[&a.kp]), "BundleSizeChanged");
    let ix = g.ix_repick_bundle(&a, R1, &first, &second);
    g.send(&[ix], &[&a.kp]).unwrap();
    assert!(g.svm.get_account(&bundle_pda(R1, &first.commitment)).map_or(true, |x| x.lamports == 0), "old bundle closed");
    assert_eq!((g.round(R1).entries, g.round(R1).pool_tokens), (entries, pool), "same picks, no new tickets");
    assert_eq!(g.lamports(&a.kp.pubkey()), a0, "rent and deposit recycled");
    let stored: Bundle = g.fetch(&bundle_pda(R1, &second.commitment));
    assert_eq!(stored.numbers, vec![9; 3]);

    let third = seal_bundle(&[3, 6, 9], &a.kp.pubkey());
    g.warp_to(CLOSE - REPICK_CUTOFF + 1);
    let ix = g.ix_repick_bundle(&a, R1, &second, &third);
    fails(g.send(&[ix], &[&a.kp]), "RepickClosed");

    g.warp_to(CLOSE);
    fails(g.reveal_bundle(&a, R1, &first), "AccountNotInitialized");
    g.reveal_bundle(&a, R1, &second).unwrap();
    assert_eq!(g.tally(R1, 6).count, 1);
    assert!(g.svm.get_account(&tally_pda(R1, 5)).map_or(true, |x| x.lamports == 0), "5 was changed away");
}

/// A bundle paid from stake: k tickets from rewards, then stake; the referrer gets k shares.
#[test]
fn a_bundle_paid_from_stake_with_a_referrer() {
    let mut g = Game::new(TIERS);
    let [a, b] = g.players::<2>();
    let one_pct = tok(5) / 100;
    g.stake(&a, tok(20)).unwrap();
    let (b0, wallet0) = (g.balance(&b.ata), g.balance(&a.ata));
    let s = seal_bundle(&[4, 6, 8], &a.kp.pubkey());
    let ix = g.ix_enter_bundle_from_stake(&a, R1, &s, Some(b.ata));
    g.send(&[ix], &[&a.kp]).unwrap();
    assert_eq!(g.staker(&a).amount, tok(5), "three tickets from a 20-token stake");
    assert_eq!(g.balance(&a.ata), wallet0, "nothing from the wallet");
    assert_eq!(g.balance(&b.ata) - b0, 3 * one_pct, "the referrer's share of each ticket");
    assert_eq!(g.round(R1).entries, 3);
    let p: PlayerProfile = g.fetch(&profile_pda(&a.kp.pubkey()));
    assert_eq!(p.referrer, b.kp.pubkey());
    let big = seal_bundle(&[1, 2], &a.kp.pubkey());
    let ix = g.ix_enter_bundle_from_stake(&a, R1, &big, Some(b.ata));
    fails(g.send(&[ix], &[&a.kp]), "NotEnoughStaked"); // 10 tokens needed, 5 staked
}