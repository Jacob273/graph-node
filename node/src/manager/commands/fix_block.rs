use graph::{
    anyhow::{bail, ensure},
    components::store::ChainStore as ChainStoreTrait,
    prelude::{
        anyhow::{self, anyhow, Context},
        web3::types::H256,
    },
    slog::Logger,
};
use graph_chain_ethereum::{EthereumAdapter, EthereumAdapterTrait};
use graph_store_postgres::ChainStore;
use std::sync::Arc;

pub async fn by_hash(
    hash: &str,
    chain_store: Arc<ChainStore>,
    ethereum_adapter: &EthereumAdapter,
    logger: &Logger,
) -> anyhow::Result<()> {
    let block_hash = helpers::parse_block_hash(hash)?;
    run(&block_hash, &chain_store, ethereum_adapter, logger).await
}

pub async fn by_number(
    number: i32,
    chain_store: Arc<ChainStore>,
    ethereum_adapter: &EthereumAdapter,
    logger: &Logger,
) -> anyhow::Result<()> {
    let block_hash = steps::resolve_block_hash_from_block_number(number, &chain_store)?;
    run(&block_hash, &chain_store, ethereum_adapter, logger).await
}

pub async fn by_range(
    chain_store: Arc<ChainStore>,
    ethereum_adapter: &EthereumAdapter,
    range: &str,
    logger: &Logger,
) -> anyhow::Result<()> {
    // Resolve a range of block numbers into a collection of blocks hashes
    let range = range.parse::<ranges::Range>()?;
    let (min, max) = range.min_max()?;
    let max = match max {
        // When we have an open upper bound, we must check the number of the chain head block
        None => steps::find_chain_head(&chain_store)?,
        Some(x) => x,
    };
    // FIXME: This performs poorly.
    // TODO: This could be turned into async code
    for block_number in min..=max {
        println!("Fixing block [{block_number}/{max}]");
        let block_hash = steps::resolve_block_hash_from_block_number(block_number, &chain_store)?;
        run(&block_hash, &chain_store, ethereum_adapter, logger).await?
    }
    Ok(())
}

pub fn truncate(chain_store: Arc<ChainStore>, skip_confirmation: bool) -> anyhow::Result<()> {
    if !skip_confirmation && !helpers::prompt_for_confirmation()? {
        println!("Aborting.");
        return Ok(());
    }

    chain_store
        .truncate_block_cache()
        .with_context(|| format!("Failed to truncate block cache for {}", chain_store.chain))
}

async fn run(
    block_hash: &H256,
    chain_store: &ChainStore,
    ethereum_adapter: &EthereumAdapter,
    logger: &Logger,
) -> anyhow::Result<()> {
    let cached_block = steps::fetch_single_cached_block(block_hash, &chain_store)?;
    let provider_block =
        steps::fetch_single_provider_block(&block_hash, ethereum_adapter, logger).await?;
    let diff = steps::diff_block_pair(&cached_block, &provider_block);
    steps::report_difference(diff.as_deref(), &block_hash);
    if diff.is_some() {
        steps::delete_block(&block_hash, &chain_store)?;
    }
    Ok(())
}

mod steps {
    use super::*;
    use futures::compat::Future01CompatExt;
    use graph::prelude::serde_json::{self, Value};
    use json_structural_diff::{colorize as diff_to_string, JsonDiff};

    /// Queries the [`ChainStore`] about the block hash for the given block number.
    ///
    /// Errors on a non-unary result.
    pub(super) fn resolve_block_hash_from_block_number(
        number: i32,
        chain_store: &ChainStore,
    ) -> anyhow::Result<H256> {
        let block_hashes = chain_store.block_hashes_by_block_number(number)?;
        helpers::get_single_item("block hash", block_hashes)
            .with_context(|| format!("Failed to locate block number {} in store", number))
    }

    /// Queries the [`ChainStore`] for a cached block given a block hash.
    ///
    /// Errors on a non-unary result.
    pub(super) fn fetch_single_cached_block(
        block_hash: &H256,
        chain_store: &ChainStore,
    ) -> anyhow::Result<Value> {
        let blocks = chain_store.blocks(&[*block_hash])?;
        if blocks.is_empty() {
            bail!("Could not find a block with hash={block_hash:?} in cache")
        }
        helpers::get_single_item("block", blocks)
            .with_context(|| format!("Failed to locate block {} in store.", block_hash))
    }

    /// Fetches a block from a JRPC endpoint.
    ///
    /// Errors on a non-unary result.
    pub(super) async fn fetch_single_provider_block(
        block_hash: &H256,
        ethereum_adapter: &EthereumAdapter,
        logger: &Logger,
    ) -> anyhow::Result<Value> {
        let provider_block = ethereum_adapter
            .block_by_hash(&logger, *block_hash)
            .compat()
            .await
            .with_context(|| format!("failed to fetch block {block_hash}"))?
            .ok_or_else(|| anyhow!("JRPC provider found no block {block_hash}"))?;
        ensure!(
            provider_block.hash == Some(*block_hash),
            "Provider responded with a different block hash"
        );
        serde_json::to_value(provider_block)
            .context("failed to parse provider block as a JSON value")
    }

    /// Compares two [`serde_json::Value`] values.
    ///
    /// If they are different, returns a user-friendly string ready to be displayed.
    pub(super) fn diff_block_pair(a: &Value, b: &Value) -> Option<String> {
        if a == b {
            None
        } else {
            match JsonDiff::diff(a, &b, false).diff {
                // The diff could potentially be a `Value::Null`, which is equivalent to not being
                // different at all.
                None | Some(Value::Null) => None,
                Some(diff) => {
                    // Convert the JSON diff to a pretty-formatted text that will be displayed to
                    // the user
                    Some(diff_to_string(&diff, false))
                }
            }
        }
    }

    /// Prints the difference between two [`serde_json::Value`] values to the user.
    pub(super) fn report_difference(difference: Option<&str>, hash: &H256) {
        if let Some(diff) = difference {
            eprintln!("block {hash} diverges from cache:");
            eprintln!("{diff}");
        } else {
            println!("Cached block is equal to the same block from provider.")
        }
    }

    /// Attempts to delete a block from the block cache.
    pub(super) fn delete_block(hash: &H256, chain_store: &ChainStore) -> anyhow::Result<()> {
        println!("Deleting block {hash} from cache.");
        chain_store.delete_blocks(&[&hash])?;
        println!("Done.");
        Ok(())
    }

    /// Queries the [`ChainStore`] about the chain head.
    pub(super) fn find_chain_head(chain_store: &ChainStore) -> anyhow::Result<i32> {
        let chain_head: Option<i32> = chain_store.chain_head_block(&chain_store.chain)?;
        chain_head.ok_or_else(|| anyhow!("Could not find the chain head for {}", chain_store.chain))
    }
}

mod helpers {
    use super::*;
    use graph::prelude::hex;
    use std::io::{self, Write};

    /// Tries to parse a [`H256`] from a hex string.
    pub(super) fn parse_block_hash(hash: &str) -> anyhow::Result<H256> {
        let hash = hash.trim_start_matches("0x");
        let hash = hex::decode(hash)
            .with_context(|| format!("Cannot parse H256 value from string `{}`", hash))?;
        Ok(H256::from_slice(&hash))
    }

    /// Asks users if they are certain about truncating the whole block cache.
    pub(super) fn prompt_for_confirmation() -> anyhow::Result<bool> {
        print!("This will delete all cached blocks.\nProceed? [y/N] ");
        io::stdout().flush()?;

        let mut answer = String::new();
        io::stdin().read_line(&mut answer)?;
        answer.make_ascii_lowercase();

        match answer.trim() {
            "y" | "yes" => Ok(true),
            _ => Ok(false),
        }
    }

    /// Convenience function for extracting values from unary sets.
    pub(super) fn get_single_item<I, T>(name: &'static str, collection: I) -> anyhow::Result<T>
    where
        I: IntoIterator<Item = T>,
    {
        let mut iterator = collection.into_iter();
        match (iterator.next(), iterator.next()) {
            (Some(a), None) => Ok(a),
            (None, None) => bail!("Expected a single {name} but found none."),
            _ => bail!("Expected a single {name} but found multiple occurrences."),
        }
    }
}

/// Custom range type that supports being parsed from a string.
mod ranges {
    use graph::prelude::anyhow::{self, bail};
    use std::str::FromStr;

    pub(super) struct Range {
        pub(super) lower_bound: Option<i32>,
        pub(super) upper_bound: Option<i32>,
        pub(super) inclusive: bool,
    }

    impl Range {
        fn new(lower_bound: Option<i32>, upper_bound: Option<i32>, inclusive: bool) -> Self {
            Self {
                lower_bound,
                upper_bound,
                inclusive,
            }
        }

        pub(super) fn min_max(&self) -> anyhow::Result<(i32, Option<i32>)> {
            let min = match self.lower_bound {
                None => 1, // When a lower bound is not set, we adjust it to the lowest possible block number
                Some(0) => anyhow::bail!("Genesis block can't be removed."),
                Some(x) if x < 0 => anyhow::bail!("Negative block number"),
                Some(x) => x,
            };
            let inclusive = if self.inclusive { 1 } else { 0 };
            let max = self.upper_bound.map(|x| x + inclusive);
            Ok((min, max))
        }
    }

    impl FromStr for Range {
        type Err = anyhow::Error;

        fn from_str(s: &str) -> Result<Self, Self::Err> {
            const INCLUSIVE: &str = "..=";
            const EXCLUSIVE: &str = "..";
            if !s.contains(INCLUSIVE) && !s.contains(EXCLUSIVE) {
                bail!("Malformed range expression")
            }
            let (separator, inclusive) = if s.contains("..=") {
                (INCLUSIVE, true)
            } else {
                (EXCLUSIVE, false)
            };
            let split: Vec<&str> = s.split(separator).collect();
            let range = match split.as_slice() {
                // open upper bounds are always inclusive
                ["", ""] => Range::new(None, None, true),
                [start, ""] => {
                    let start: i32 = start.parse::<i32>()?;
                    // open upper bounds are always inclusive
                    Range::new(Some(start), None, true)
                }
                ["", end] => {
                    let end = end.parse::<i32>()?;
                    Range::new(None, Some(end), inclusive)
                }
                [start, end] => {
                    let start: i32 = start.parse::<i32>()?;
                    let end: i32 = end.parse::<i32>()?;
                    if start > end {
                        bail!("Invalid range")
                    }
                    Range::new(Some(start), Some(end), inclusive)
                }
                _ => bail!("Invalid range"),
            };
            Ok(range)
        }
    }
}
