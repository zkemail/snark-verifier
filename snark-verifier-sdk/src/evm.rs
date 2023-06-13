use super::{CircuitExt, Plonk};
#[cfg(feature = "display")]
use ark_std::{end_timer, start_timer};
use ethereum_types::Address;
use halo2_base::halo2_proofs::{
    dev::MockProver,
    halo2curves::bn256::{Bn256, Fq, Fr, G1Affine},
    plonk::{create_proof, verify_proof, Circuit, ProvingKey, VerifyingKey},
    poly::{
        commitment::{Params, ParamsProver, Prover, Verifier},
        kzg::{
            commitment::{KZGCommitmentScheme, ParamsKZG},
            msm::DualMSM,
            multiopen::{ProverGWC, ProverSHPLONK, VerifierGWC, VerifierSHPLONK},
            strategy::{AccumulatorStrategy, GuardKZG},
        },
        VerificationStrategy,
    },
    transcript::{TranscriptReadBuffer, TranscriptWriterBuffer},
};
use itertools::Itertools;
use rand::Rng;
pub use snark_verifier::loader::evm::encode_calldata;
use snark_verifier::{
    loader::evm::{compile_yul, EvmLoader, ExecutorBuilder},
    pcs::{
        kzg::{Bdfg21, Gwc19, Kzg, KzgAccumulator, KzgDecidingKey, KzgSuccinctVerifyingKey},
        Decider, MultiOpenScheme, PolynomialCommitmentScheme,
    },
    system::halo2::{compile, transcript::evm::EvmTranscript, Config},
    verifier::PlonkVerifier,
};
use std::{fs, io, path::Path, rc::Rc};

/// Generates a proof for evm verification using either SHPLONK or GWC proving method. Uses Keccak for Fiat-Shamir.
pub fn gen_evm_proof<'params, C, P, V>(
    params: &'params ParamsKZG<Bn256>,
    pk: &'params ProvingKey<G1Affine>,
    circuit: C,
    instances: Vec<Vec<Fr>>,
    rng: &mut (impl Rng + Send),
) -> Vec<u8>
where
    C: Circuit<Fr>,
    P: Prover<'params, KZGCommitmentScheme<Bn256>>,
    V: Verifier<
        'params,
        KZGCommitmentScheme<Bn256>,
        Guard = GuardKZG<'params, Bn256>,
        MSMAccumulator = DualMSM<'params, Bn256>,
    >,
{
    #[cfg(debug_assertions)]
    {
        MockProver::run(params.k(), &circuit, instances.clone()).unwrap().assert_satisfied();
    }

    let instances = instances.iter().map(|instances| instances.as_slice()).collect_vec();

    #[cfg(feature = "display")]
    let proof_time = start_timer!(|| "Create EVM proof");
    let proof = {
        let mut transcript = TranscriptWriterBuffer::<_, G1Affine, _>::init(Vec::new());
        create_proof::<KZGCommitmentScheme<Bn256>, P, _, _, EvmTranscript<_, _, _, _>, _>(
            params,
            pk,
            &[circuit],
            &[instances.as_slice()],
            rng,
            &mut transcript,
        )
        .unwrap();
        transcript.finalize()
    };
    #[cfg(feature = "display")]
    end_timer!(proof_time);

    let accept = {
        let mut transcript = TranscriptReadBuffer::<_, G1Affine, _>::init(proof.as_slice());
        VerificationStrategy::<_, V>::finalize(
            verify_proof::<_, V, _, EvmTranscript<_, _, _, _>, _>(
                params.verifier_params(),
                pk.get_vk(),
                AccumulatorStrategy::new(params.verifier_params()),
                &[instances.as_slice()],
                &mut transcript,
            )
            .unwrap(),
        )
    };
    assert!(accept);

    proof
}

pub fn gen_evm_proof_gwc<'params, C: Circuit<Fr>>(
    params: &'params ParamsKZG<Bn256>,
    pk: &'params ProvingKey<G1Affine>,
    circuit: C,
    instances: Vec<Vec<Fr>>,
    rng: &mut (impl Rng + Send),
) -> Vec<u8> {
    gen_evm_proof::<C, ProverGWC<_>, VerifierGWC<_>>(params, pk, circuit, instances, rng)
}

pub fn gen_evm_proof_shplonk<'params, C: Circuit<Fr>>(
    params: &'params ParamsKZG<Bn256>,
    pk: &'params ProvingKey<G1Affine>,
    circuit: C,
    instances: Vec<Vec<Fr>>,
    rng: &mut (impl Rng + Send),
) -> Vec<u8> {
    gen_evm_proof::<C, ProverSHPLONK<_>, VerifierSHPLONK<_>>(params, pk, circuit, instances, rng)
}

pub fn gen_evm_verifier<C, PCS>(
    params: &ParamsKZG<Bn256>,
    vk: &VerifyingKey<G1Affine>,
    num_instance: Vec<usize>,
    path: Option<&Path>,
) -> Vec<u8>
where
    C: CircuitExt<Fr>,
    PCS: PolynomialCommitmentScheme<
            G1Affine,
            Rc<EvmLoader>,
            Accumulator = KzgAccumulator<G1Affine, Rc<EvmLoader>>,
        > + MultiOpenScheme<
            G1Affine,
            Rc<EvmLoader>,
            SuccinctVerifyingKey = KzgSuccinctVerifyingKey<G1Affine>,
        > + Decider<G1Affine, Rc<EvmLoader>, DecidingKey = KzgDecidingKey<Bn256>>,
{
    let svk = params.get_g()[0].into();
    let dk = (params.g2(), params.s_g2()).into();
    let protocol = compile(
        params,
        vk,
        Config::kzg()
            .with_num_instance(num_instance.clone())
            .with_accumulator_indices(C::accumulator_indices()),
    );

    let loader = EvmLoader::new::<Fq, Fr>();
    let protocol = protocol.loaded(&loader);
    let mut transcript = EvmTranscript::<_, Rc<EvmLoader>, _, _>::new(&loader);

    let instances = transcript.load_instances(num_instance);
    let proof = Plonk::<PCS>::read_proof(&svk, &protocol, &instances, &mut transcript);
    Plonk::<PCS>::verify(&svk, &dk, &protocol, &instances, &proof);

    let yul_code = loader.yul_code();
    let byte_code = compile_yul(&yul_code);
    if let Some(path) = path {
        path.parent().and_then(|dir| fs::create_dir_all(dir).ok()).unwrap();
        fs::write(path, yul_code).unwrap();
    }
    byte_code
}

pub fn gen_evm_verifier_gwc<C: CircuitExt<Fr>>(
    params: &ParamsKZG<Bn256>,
    vk: &VerifyingKey<G1Affine>,
    num_instance: Vec<usize>,
    path: Option<&Path>,
) -> Vec<u8> {
    gen_evm_verifier::<C, Kzg<Bn256, Gwc19>>(params, vk, num_instance, path)
}

pub fn gen_evm_verifier_shplonk<C: CircuitExt<Fr>>(
    params: &ParamsKZG<Bn256>,
    vk: &VerifyingKey<G1Affine>,
    num_instance: Vec<usize>,
    path: Option<&Path>,
) -> Vec<u8> {
    gen_evm_verifier::<C, Kzg<Bn256, Bdfg21>>(params, vk, num_instance, path)
}

pub fn evm_verify(
    deployment_code: Vec<u8>,
    limit_contract_code_size: bool,
    instances: Vec<Vec<Fr>>,
    proof: Vec<u8>,
) {
    let calldata = encode_calldata(&instances, &proof);
    let success = {
        let mut evm = ExecutorBuilder::default().with_gas_limit(u64::MAX.into());
        if !limit_contract_code_size {
            evm = evm.with_contract_code_size_limit(usize::MAX);
        }
        let mut evm = evm.build();

        let caller = Address::from_low_u64_be(0xfe);
        let verifier = evm.deploy(caller, deployment_code.into(), 0.into()).address.unwrap();
        let result = evm.call_raw(caller, verifier, calldata.into(), 0.into());

        dbg!(result.gas_used);

        !result.reverted
    };
    assert!(success);
}

pub fn write_calldata(instances: &[Vec<Fr>], proof: &[u8], path: &Path) -> io::Result<String> {
    let calldata = encode_calldata(instances, proof);
    let calldata = hex::encode(calldata);
    fs::write(path, &calldata)?;
    Ok(calldata)
}
