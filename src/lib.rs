pub mod blockstore;
pub mod ext;
pub mod state;
pub mod types;
mod utils;

use anyhow::anyhow;
use cid::Cid;
use ext::sca::SCA_ACTOR_ADDR;
use fil_actor_hierarchical_sca::{Checkpoint, FundParams, Method, MIN_COLLATERAL_AMOUNT};
use fvm_ipld_encoding::{RawBytes, DAG_CBOR};
use fvm_sdk as sdk;
use fvm_sdk::NO_DATA_BLOCK_ID;
use fvm_shared::actor::builtin::Type;
use fvm_shared::address::Address;
use fvm_shared::bigint::bigint_ser::BigIntDe;
use fvm_shared::econ::TokenAmount;
use fvm_shared::{ActorID, METHOD_SEND};
use num_traits::Zero;
use sdk::actor::get_actor_code_cid;
use state::get_votes;

use crate::blockstore::*;
use crate::state::{get_stake, State};
use crate::types::*;
use crate::utils::*;

pub const TEST_ADDR_ID: ActorID = 339;

/// The actor's WASM entrypoint. It takes the ID of the parameters block,
/// and returns the ID of the return value block, or NO_DATA_BLOCK_ID if no
/// return value.
///
/// Should probably have macros similar to the ones on fvm.filecoin.io snippets.
/// Put all methods inside an impl struct and annotate it with a derive macro
/// that handles state serde and dispatch.
#[no_mangle]
pub fn invoke(params: u32) -> u32 {
    let params = sdk::message::params_raw(params).unwrap().1;
    let params = RawBytes::new(params);
    // Conduct method dispatch. Handle input parameters and return data.
    let ret: anyhow::Result<Option<RawBytes>> = match sdk::message::method_number() {
        1 => Actor::constructor(deserialize_params(&params).unwrap()),
        2 => Actor::join(deserialize_params(&params).unwrap()),
        3 => Actor::leave(),
        4 => Actor::kill(),
        5 => Actor::submit_checkpoint(deserialize_params(&params).unwrap()),
        _ => abort!(USR_UNHANDLED_MESSAGE, "unrecognized method"),
    };

    // Insert the return data block if necessary, and return the correct
    // block ID.
    match ret {
        Ok(None) => NO_DATA_BLOCK_ID,
        Ok(Some(v)) => match sdk::ipld::put_block(DAG_CBOR, v.bytes()) {
            Ok(id) => id,
            Err(e) => abort!(USR_SERIALIZATION, "failed to store return value: {}", e),
        },
        Err(e) => abort!(USR_ILLEGAL_STATE, "error calling method: {}", e),
    }
}

/// SubnetActor trait. Custom subnet actors need to implement this trait
/// in order to be used as part of hierarchical consensus.
///
/// Subnet actors are responsible for the governing policies of HC subnets.
pub trait SubnetActor {
    /// Deploys subnet actor with the corresponding parameters.
    fn constructor(params: ConstructParams) -> anyhow::Result<Option<RawBytes>>;
    /// Logic for new peers to join a subnet.
    fn join(params: JoinParams) -> anyhow::Result<Option<RawBytes>>;
    /// Called by peers to leave a subnet.
    fn leave() -> anyhow::Result<Option<RawBytes>>;
    /// Sends a kill signal for the subnet to the SCA.
    fn kill() -> anyhow::Result<Option<RawBytes>>;
    /// Submits a new checkpoint for the subnet.
    fn submit_checkpoint(ch: Checkpoint) -> anyhow::Result<Option<RawBytes>>;
}

pub struct Actor;

impl SubnetActor for Actor {
    /// The constructor populates the initial state.
    ///
    /// Method num 1. This is part of the Filecoin calling convention.
    /// InitActor#Exec will call the constructor on method_num = 1.
    fn constructor(params: ConstructParams) -> anyhow::Result<Option<RawBytes>> {
        // This constant should be part of the SDK.
        const INIT_ACTOR_ADDR: ActorID = 1;

        // Should add SDK sugar to perform ACL checks more succinctly.
        // i.e. the equivalent of the validate_* builtin-actors runtime methods.
        // https://github.com/filecoin-project/builtin-actors/blob/master/actors/runtime/src/runtime/fvm.rs#L110-L146
        let is_test = State::is_test();
        if sdk::message::caller() != INIT_ACTOR_ADDR
            && (sdk::message::caller() != TEST_ADDR_ID && is_test)
        {
            abort!(USR_FORBIDDEN, "constructor invoked by non-init actor");
        }

        let state = State::new(params, is_test);
        state.save();
        Ok(None)
    }

    /// Called by peers looking to join a subnet.
    ///
    /// It implements the basic logic to onboard new peers to the subnet.
    fn join(params: JoinParams) -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();
        let caller = Address::new_id(sdk::message::caller());
        // check type of caller
        let code_cid = get_actor_code_cid(&caller).unwrap_or(Cid::default());
        if sdk::actor::get_builtin_actor_type(&code_cid) != Some(Type::Account) {
            abort!(USR_FORBIDDEN, "caller not account actor type");
        }

        let amount = sdk::message::value_received();
        if amount <= TokenAmount::zero() {
            abort!(
                USR_ILLEGAL_ARGUMENT,
                "a minimum collateral is required to join the subnet"
            );
        }
        // increase collateral
        st.add_stake(&caller, &params.validator_net_addr, &amount)?;
        // if we have enough collateral, register in SCA
        if st.status == Status::Instantiated {
            if sdk::sself::current_balance() >= TokenAmount::from(MIN_COLLATERAL_AMOUNT) {
                st.send(
                    &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                    Method::Register as u64,
                    RawBytes::default(),
                    st.total_stake.clone(),
                )?;
            }
        } else {
            st.send(
                &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                Method::AddStake as u64,
                RawBytes::default(),
                amount,
            )?;
        }
        st.mutate_state();
        st.save();
        Ok(None)
    }

    /// Called by peers looking to leave a subnet.
    fn leave() -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();
        let caller = Address::new_id(sdk::message::caller());
        // check type of caller
        let code_cid = get_actor_code_cid(&caller).unwrap_or(Cid::default());
        if sdk::actor::get_builtin_actor_type(&code_cid) != Some(Type::Account) {
            abort!(USR_FORBIDDEN, "caller not account actor type");
        }

        // get stake to know how much to release
        let bt = make_map_with_root::<_, BigIntDe>(&st.stake, &Blockstore)?;
        let stake = get_stake(&bt, &caller.clone())?;
        if stake == TokenAmount::zero() {
            abort!(USR_ILLEGAL_STATE, "caller has no stake in subnet");
        }

        // release from SCA
        if st.status != Status::Terminating {
            st.send(
                &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
                Method::ReleaseStake as u64,
                RawBytes::serialize(FundParams {
                    value: stake.clone(),
                })?,
                TokenAmount::zero(),
            )?;
        }

        // remove stake from balance table
        st.rm_stake(&caller, &stake)?;

        // send back to owner
        st.send(&caller, METHOD_SEND, RawBytes::default(), stake)?;

        st.mutate_state();
        st.save();
        Ok(None)
    }

    fn kill() -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();

        if st.status == Status::Terminating || st.status == Status::Killed {
            abort!(
                USR_ILLEGAL_STATE,
                "the subnet is already in a killed or terminating state"
            );
        }
        if st.validator_set.len() != 0 {
            abort!(
                USR_ILLEGAL_STATE,
                "this subnet can only be killed when all validators have left"
            );
        }

        // move to terminating state
        st.status = Status::Terminating;

        // unregister subnet
        st.send(
            &Address::new_id(ext::sca::SCA_ACTOR_ADDR),
            Method::Kill as u64,
            RawBytes::default(),
            TokenAmount::zero(),
        )?;

        st.mutate_state();
        st.save();
        Ok(None)
    }

    /// SubmitCheckpoint accepts signed checkpoint votes for miners.
    ///
    /// This functions verifies that the checkpoint is valid before
    /// propagating it for commitment to the SCA. It expects at least
    /// votes from 2/3 of miners with collateral.
    fn submit_checkpoint(checkpoint: Checkpoint) -> anyhow::Result<Option<RawBytes>> {
        let mut st = State::load();
        let caller = Address::new_id(sdk::message::caller());
        // check type of caller
        let code_cid = get_actor_code_cid(&caller).unwrap_or(Cid::default());
        if sdk::actor::get_builtin_actor_type(&code_cid) != Some(Type::Account) {
            abort!(USR_FORBIDDEN, "caller not account actor type");
        }

        let ch_cid = checkpoint.cid();
        // verify checkpoint
        st.verify_checkpoint(&checkpoint)?;

        // get votes for committed checkpoint
        let mut votes_map = make_map_with_root::<_, Votes>(&st.window_checks, &Blockstore)
            .map_err(|e| anyhow!("failed to load checkpoints: {}", e))?;
        let mut found = false;
        let mut votes = match get_votes(&votes_map, &ch_cid)? {
            Some(v) => {
                found = true;
                v.clone()
            }
            None => Votes {
                validators: Vec::new(),
            },
        };

        if votes.validators.iter().any(|x| x == &caller) {
            return Err(anyhow!("miner has already voted the checkpoint"));
        }

        // add miner vote
        votes.validators.push(caller);

        // if has majority
        if st.has_majority_vote(&votes)? {
            // commit checkpoint
            st.flush_checkpoint::<&Blockstore>(&checkpoint)?;
            // propagate to sca
            st.send(
                &Address::new_id(SCA_ACTOR_ADDR),
                Method::CommitChildCheckpoint as u64,
                RawBytes::serialize(checkpoint)?,
                0.into(),
            )?;
            // remove votes used for commitment
            if found {
                votes_map.delete(&ch_cid.to_bytes())?;
            }
        } else {
            // if no majority store vote and return
            votes_map.set(ch_cid.to_bytes().into(), votes)?;
        }

        // flush votes
        st.window_checks = votes_map.flush()?;

        st.save();
        Ok(None)
    }
}
