use indicatif::{MultiProgress, ProgressBar, ProgressStyle};
use log::{error, warn};
use rusqlite::{params, Connection};
use solana_sdk::program_pack::Pack;
use solana_snapshot_etl::append_vec::StoredAccountMeta;
use solana_snapshot_etl::{SnapshotError, StoredAccountMetaHandle};
use std::iter::Iterator;
use std::path::{Path, PathBuf};

pub(crate) type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub(crate) struct SqliteIndexer {
    db: Connection,
    db_path: PathBuf,
    db_temp_guard: TempFileGuard,

    multi_progress: MultiProgress,
    accounts_counter: ProgressCounter,
    token_accounts_counter: ProgressCounter,
}

pub(crate) struct IndexStats {
    pub(crate) accounts_total: u64,
    pub(crate) token_accounts_total: u64,
}

impl SqliteIndexer {
    pub(crate) fn new(db_path: PathBuf) -> Result<Self> {
        // Create temporary DB file, which gets promoted on success.
        let temp_file_name = format!("_{}.tmp", db_path.file_name().unwrap().to_string_lossy());
        let temp_db_path = db_path.with_file_name(&temp_file_name);
        let _ = std::fs::remove_file(&temp_db_path);
        let db_temp_guard = TempFileGuard::new(temp_db_path.clone());

        // Open database.
        let db = Self::create_db(&temp_db_path)?;

        // Create progress bars.
        let spinner_style = ProgressStyle::with_template(
            "{prefix:>10.bold.dim} {spinner} rate={per_sec}/s total={human_pos}",
        )
        .unwrap();
        let multi_progress = MultiProgress::new();
        let accounts_spinner = multi_progress.add(
            ProgressBar::new_spinner()
                .with_style(spinner_style.clone())
                .with_prefix("accs"),
        );
        let accounts_counter = ProgressCounter::new(accounts_spinner);
        let token_accounts_spinner = multi_progress.add(
            ProgressBar::new_spinner()
                .with_style(spinner_style)
                .with_prefix("token_accs"),
        );
        let token_accounts_counter = ProgressCounter::new(token_accounts_spinner);

        Ok(Self {
            db,
            db_path,
            db_temp_guard,

            multi_progress,
            accounts_counter,
            token_accounts_counter,
        })
    }

    fn create_db(path: &Path) -> Result<Connection> {
        let db = Connection::open(&path)?;
        db.pragma_update(None, "synchronous", false)?;
        db.pragma_update(None, "journal_mode", "off")?;
        db.pragma_update(None, "locking_mode", "exclusive")?;
        db.execute(
            "\
CREATE TABLE token_mint (
    pubkey BLOB(32) NOT NULL PRIMARY KEY,
    mint_authority BLOB(32) NULL,
    supply INTEGER(8) NOT NULL,
    decimals INTEGER(2) NOT NULL,
    is_initialized BOOL NOT NULL,
    freeze_authority BLOB(32) NULL
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_account (
    pubkey BLOB(32) NOT NULL PRIMARY KEY,
    mint BLOB(32) NOT NULL,
    owner BLOB(32) NOT NULL,
    amount INTEGER(8) NOT NULL,
    delegate BLOB(32),
    state INTEGER(1) NOT NULL,
    is_native INTEGER(8),
    delegated_amount INTEGER(8) NOT NULL,
    close_authority BLOB(32)
);",
            [],
        )?;
        db.execute(
            "\
CREATE TABLE token_multisig (
    pubkey BLOB(32) NOT NULL,
    signer BLOB(32) NOT NULL,
    m INTEGER(2) NOT NULL,
    n INTEGER(2) NOT NULL,
    PRIMARY KEY (pubkey, signer)
);",
            [],
        )?;
        Ok(db)
    }

    pub(crate) fn insert_all<I>(mut self, mut iterator: I) -> Result<IndexStats>
    where
        I: Iterator<Item = std::result::Result<StoredAccountMetaHandle, SnapshotError>>,
    {
        iterator.try_for_each(|account| {
            let account = account?;
            let account = account.access().unwrap();
            self.insert_account(&account)
        })?;
        self.finish()
    }

    fn finish(mut self) -> Result<IndexStats> {
        let stats = IndexStats {
            accounts_total: self.accounts_counter.counter,
            token_accounts_total: self.token_accounts_counter.counter,
        };
        self.db_temp_guard.promote(self.db_path)?;
        let _ = &self.multi_progress;
        Ok(stats)
    }

    fn insert_account(&mut self, account: &StoredAccountMeta) -> Result<()> {
        if account.account_meta.owner == spl_token::id() {
            self.insert_token(account)?;
        }
        self.accounts_counter.inc();
        Ok(())
    }

    fn insert_token(&mut self, account: &StoredAccountMeta) -> Result<()> {
        match account.meta.data_len as usize {
            spl_token::state::Account::LEN => {
                let token_account = spl_token::state::Account::unpack(account.data);
                if let Ok(token_account) = token_account {
                    self.insert_token_account(account, &token_account)?;
                }
            }
            spl_token::state::Mint::LEN => {
                let token_mint = spl_token::state::Mint::unpack(account.data);
                if let Ok(token_mint) = token_mint {
                    self.insert_token_mint(account, &token_mint)?;
                }
            }
            spl_token::state::Multisig::LEN => {
                let token_multisig = spl_token::state::Multisig::unpack(account.data);
                if let Ok(token_multisig) = token_multisig {
                    self.insert_token_multisig(account, &token_multisig)?;
                }
            }
            _ => {
                warn!(
                    "Token program account {} has unexpected size {}",
                    account.meta.pubkey, account.meta.data_len
                );
                return Ok(());
            }
        }
        self.token_accounts_counter.inc();
        Ok(())
    }

    fn insert_token_account(
        &mut self,
        account: &StoredAccountMeta,
        token_account: &spl_token::state::Account,
    ) -> Result<()> {
        let mut token_account_insert = self.db.prepare_cached("\
INSERT OR REPLACE INTO token_account (pubkey, mint, owner, amount, delegate, state, is_native, delegated_amount, close_authority)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?);")?;
        token_account_insert.insert(params![
            account.meta.pubkey.as_ref(),
            token_account.mint.as_ref(),
            token_account.owner.as_ref(),
            token_account.amount as i64,
            Option::<[u8; 32]>::from(token_account.delegate.map(|key| key.to_bytes())),
            token_account.state as u8,
            Option::<u64>::from(token_account.is_native),
            token_account.delegated_amount as i64,
            Option::<[u8; 32]>::from(token_account.close_authority.map(|key| key.to_bytes())),
        ])?;
        Ok(())
    }

    fn insert_token_mint(
        &mut self,
        account: &StoredAccountMeta,
        token_mint: &spl_token::state::Mint,
    ) -> Result<()> {
        let mut token_mint_insert = self.db.prepare_cached("\
INSERT OR REPLACE INTO token_mint (pubkey, mint_authority, supply, decimals, is_initialized, freeze_authority)
    VALUES (?, ?, ?, ?, ?, ?);")?;
        token_mint_insert.insert(params![
            account.meta.pubkey.as_ref(),
            Option::<[u8; 32]>::from(token_mint.mint_authority.map(|key| key.to_bytes()),),
            token_mint.supply as i64,
            token_mint.decimals,
            token_mint.is_initialized,
            Option::<[u8; 32]>::from(token_mint.freeze_authority.map(|key| key.to_bytes())),
        ])?;
        Ok(())
    }

    fn insert_token_multisig(
        &mut self,
        account: &StoredAccountMeta,
        token_multisig: &spl_token::state::Multisig,
    ) -> Result<()> {
        let mut token_multisig_insert = self.db.prepare_cached(
            "\
INSERT OR REPLACE INTO token_multisig (pubkey, signer, m, n)
    VALUES (?, ?, ?, ?);",
        )?;
        for signer in &token_multisig.signers[..token_multisig.n as usize] {
            token_multisig_insert.insert(params![
                account.meta.pubkey.as_ref(),
                signer.as_ref(),
                token_multisig.m,
                token_multisig.n
            ])?;
        }
        Ok(())
    }
}

struct ProgressCounter {
    progress_bar: ProgressBar,
    counter: u64,
}

impl ProgressCounter {
    fn new(progress_bar: ProgressBar) -> Self {
        Self {
            progress_bar,
            counter: 0u64,
        }
    }

    fn inc(&mut self) {
        self.counter += 1;
        if self.counter % 1024 == 0 {
            self.progress_bar.set_position(self.counter)
        }
    }
}

impl Drop for ProgressCounter {
    fn drop(&mut self) {
        self.progress_bar.set_position(self.counter);
        self.progress_bar.finish();
    }
}

struct TempFileGuard {
    pub path: Option<PathBuf>,
}

impl TempFileGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn promote<P: AsRef<Path>>(&mut self, new_name: P) -> std::io::Result<()> {
        std::fs::rename(
            self.path.take().expect("cannot promote non-existent file"),
            new_name,
        )
    }
}

impl Drop for TempFileGuard {
    fn drop(&mut self) {
        if let Some(path) = &self.path {
            if let Err(e) = std::fs::remove_file(path) {
                error!("Failed to remove temp DB: {}", e);
            }
        }
    }
}