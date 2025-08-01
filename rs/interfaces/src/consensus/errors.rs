use ic_types::consensus::idkg::{
    common::TranscriptOperationError,
    ecdsa::{PreSignatureQuadrupleError, ThresholdEcdsaSigInputsError},
    schnorr::{PreSignatureTranscriptError, ThresholdSchnorrSigInputsError},
    TranscriptParamsError,
};

use crate::crypto::ErrorReproducibility;

impl ErrorReproducibility for ThresholdEcdsaSigInputsError {
    fn is_reproducible(&self) -> bool {
        match self {
            ThresholdEcdsaSigInputsError::PreSignatureQuadruple(err) => err.is_reproducible(),
            ThresholdEcdsaSigInputsError::KeyTranscript(_) => true,
            ThresholdEcdsaSigInputsError::Failed(err) => err.is_reproducible(),
        }
    }
}

impl ErrorReproducibility for PreSignatureQuadrupleError {
    fn is_reproducible(&self) -> bool {
        match self {
            PreSignatureQuadrupleError::KappaUnmasked(_) => true,
            PreSignatureQuadrupleError::LambdaMasked(_) => true,
            PreSignatureQuadrupleError::KappaTimesLambda(_) => true,
            PreSignatureQuadrupleError::KeyTimesLambda(_) => true,
            PreSignatureQuadrupleError::Failed(err) => err.is_reproducible(),
        }
    }
}

impl ErrorReproducibility for ThresholdSchnorrSigInputsError {
    fn is_reproducible(&self) -> bool {
        match self {
            ThresholdSchnorrSigInputsError::PreSignatureTranscript(err) => err.is_reproducible(),
            ThresholdSchnorrSigInputsError::KeyTranscript(_) => true,
            ThresholdSchnorrSigInputsError::Failed(err) => err.is_reproducible(),
        }
    }
}

impl ErrorReproducibility for PreSignatureTranscriptError {
    fn is_reproducible(&self) -> bool {
        match self {
            PreSignatureTranscriptError::BlinderUnmasked(_) => true,
            PreSignatureTranscriptError::Failed(err) => err.is_reproducible(),
        }
    }
}

impl ErrorReproducibility for TranscriptParamsError {
    fn is_reproducible(&self) -> bool {
        match self {
            TranscriptParamsError::OperationRef(err) => err.is_reproducible(),
            TranscriptParamsError::ParamsValidation(err) => err.is_reproducible(),
        }
    }
}

impl ErrorReproducibility for TranscriptOperationError {
    fn is_reproducible(&self) -> bool {
        match self {
            TranscriptOperationError::ReshareOfMasked(_) => true,
            TranscriptOperationError::ReshareOfUnmasked(_) => true,
            TranscriptOperationError::UnmaskedTimesMasked1(_) => true,
            TranscriptOperationError::UnmaskedTimesMasked2(_) => true,
        }
    }
}
