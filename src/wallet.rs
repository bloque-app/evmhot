use alloy::primitives::Address;
use alloy::signers::local::coins_bip39::English;
use alloy::signers::local::MnemonicBuilder;
use anyhow::Result;

#[derive(Clone)]
pub struct Wallet {
    mnemonic: String,
}

impl Wallet {
    pub fn new(mnemonic: String) -> Self {
        Self { mnemonic }
    }

    pub fn derive_address(&self, index: u32) -> Result<Address> {
        let builder = MnemonicBuilder::<English>::default()
            .phrase(self.mnemonic.as_str())
            .index(index)?;

        let signer = builder.build()?;
        Ok(signer.address())
    }

    pub fn get_signer(&self, index: u32) -> Result<alloy::signers::local::PrivateKeySigner> {
        let builder = MnemonicBuilder::<English>::default()
            .phrase(self.mnemonic.as_str())
            .index(index)?;

        Ok(builder.build()?)
    }
}
