//! Pons launchpad contracts on Robinhood Chain: addresses, selectors, topics, calldata decoders.
//! Mirrors cli/chain.js, cli/pons.js, and cli/trade.js so the two sides never disagree.

use alloy::primitives::{Address, B256, U256, address, b256};
use alloy::sol;
use alloy::sol_types::SolCall;

pub const CHAIN_ID: u64 = 4663;
pub const FACTORY: Address = address!("7eD598BcEf8bd9Edd8C97A195C6d13f40801EC7e");
pub const LAUNCH_ROUTER: Address = address!("e33E9E479dF8802cb0866d5d05258bEc4cF62948");
pub const UNIVERSAL_ROUTER: Address = address!("8876789976decbfcbbbe364623c63652db8c0904");

/// event topics, copied from cli/chain.js
pub const T_LAUNCH: B256 = b256!("8d4aad4953d0ca700d468f3753aa14432d1b35b43ec6409f051fb6aa43a89607");
pub const T_GRAD: B256 = b256!("0a44ef75df69c534f43cd6c1aa3ef8983065fe5fe79ef9e79f6494e6f258c259");
pub const T_SWEPT: B256 = b256!("cdb72f157fd3666758a6ce201387ffb52038c7562e4fff352828da1096c4b6b4");
pub const T_BUY: B256 = b256!("ec36bf571f136799e8dc0b0b8bea4b04d8bd3d43de838aab0d5fc21d4cbfc455");
pub const T_SELL: B256 = b256!("8113d738abdcb6b38357e9d53a54a7157861a09031b453651f0fe7fe151f59df");
pub const T_TRANSFER: B256 = b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

sol! {
    struct Socials { string twitter; string telegram; string discord; string website; string farcaster; }
    struct TokenParams {
        string name; string symbol; string logo; string description; Socials socials;
        address creatorFeeRecipient; uint16 creatorTaxBps; bool buybackEnabled; bytes32 expectedEconomics; bytes32 salt;
    }
    /// LaunchRouter.launchAndBuy, selector 0xf85f8e41
    function launchAndBuy(TokenParams params, uint256 launchConfigId, address pairToken, uint256 quoteIn, uint256 minTokensOut, address recipient, address[] snipeTaxExemptions) payable returns (address token, address curve, uint256 tokensOut);
    /// Curve.buy / Curve.sell
    function buy(uint256 quoteIn, uint256 minTokensOut, address recipient) payable returns (uint256 tokensOut);
    function sell(uint256 tokensIn, uint256 minQuoteOut, address recipient) returns (uint256 quoteOut);
    function currentSnipeTaxBps(address who) view returns (uint256);
    function realQuoteReserve() view returns (uint256);
    function getReserves() view returns (uint256 quoteReserve, uint256 tokenReserve);
    function launchedAt() view returns (uint256);
    function transfer(address to, uint256 amount) returns (bool);
}

pub const SEL_LAUNCH_AND_BUY: [u8; 4] = launchAndBuyCall::SELECTOR;
pub const SEL_BUY: [u8; 4] = buyCall::SELECTOR;
pub const SEL_SELL: [u8; 4] = sellCall::SELECTOR;
pub const SEL_TRANSFER: [u8; 4] = transferCall::SELECTOR;

/// A launchAndBuy seen in the feed, with everything the scorer needs already decoded.
#[derive(Debug, Clone)]
pub struct LaunchCall {
    pub name: String,
    pub symbol: String,
    pub creator_tax_bps: u16,
    pub pair_token: Address,
    pub quote_in: U256,
    pub recipient: Address,
    pub exemptions: Vec<Address>,
}

pub fn decode_launch(input: &[u8]) -> Option<LaunchCall> {
    if input.len() < 4 || input[..4] != SEL_LAUNCH_AND_BUY { return None; }
    let c = launchAndBuyCall::abi_decode(input).ok()?;
    Some(LaunchCall {
        name: c.params.name, symbol: c.params.symbol, creator_tax_bps: c.params.creatorTaxBps,
        pair_token: c.pairToken, quote_in: c.quoteIn, recipient: c.recipient, exemptions: c.snipeTaxExemptions,
    })
}

#[derive(Debug, Clone, Copy)]
pub struct SellCall { pub tokens_in: U256, pub min_quote_out: U256, pub recipient: Address }

pub fn decode_sell(input: &[u8]) -> Option<SellCall> {
    if input.len() < 4 || input[..4] != SEL_SELL { return None; }
    let c = sellCall::abi_decode(input).ok()?;
    Some(SellCall { tokens_in: c.tokensIn, min_quote_out: c.minQuoteOut, recipient: c.recipient })
}

pub fn encode_buy(quote_in: U256, min_tokens_out: U256, recipient: Address) -> Vec<u8> {
    buyCall { quoteIn: quote_in, minTokensOut: min_tokens_out, recipient }.abi_encode()
}
pub fn encode_sell(tokens_in: U256, min_quote_out: U256, recipient: Address) -> Vec<u8> {
    sellCall { tokensIn: tokens_in, minQuoteOut: min_quote_out, recipient }.abi_encode()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn selectors_match_the_js_side() {
        assert_eq!(hex::encode(SEL_LAUNCH_AND_BUY), "f85f8e41");
    }
    #[test]
    fn sell_roundtrip() {
        let a = Address::repeat_byte(0x11);
        let bytes = encode_sell(U256::from(5u64), U256::from(1u64), a);
        let s = decode_sell(&bytes).unwrap();
        assert_eq!(s.tokens_in, U256::from(5u64));
        assert_eq!(s.recipient, a);
    }
}
