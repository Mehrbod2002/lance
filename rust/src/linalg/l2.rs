// Copyright 2023 Lance Developers.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! L2 (Euclidean) distance.
//!

use std::iter::Sum;
use std::sync::Arc;

use arrow_array::Float32Array;
use num_traits::real::Real;

/// Calculate the L2 distance between two vectors.
///
pub trait L2 {
    type Output;

    /// Calculate the L2 distance between two vectors.
    fn l2(&self, other: &Self) -> Self::Output;
}

/// Calculate the L2 distance between two vectors, using scalar operations.
///
/// Rely on compiler auto-vectorization.
#[inline]
fn l2_scalar<T: Real + Sum>(from: &[T], to: &[T]) -> T {
    from.iter()
        .zip(to.iter())
        .map(|(a, b)| (a.sub(*b).powi(2)))
        .sum::<T>()
}

impl L2 for [f32] {
    type Output = f32;

    #[inline]
    fn l2(&self, other: &[f32]) -> f32 {
        #[cfg(target_arch = "x86_64")]
        {
            // TODO: Only known platform that does not support FMA is Github Action Mac(Intel) Runner.
            // However, it introduces one more branch, which may affect performance.
            if is_x86_feature_detected!("avx2") {
                // AVX2 / FMA is the lowest x86_64 CPU requirement (released from 2011) for Lance.
                use x86_64::avx::l2_f32;
                return l2_f32(self, other);
            }
        }

        #[cfg(any(target_arch = "aarch64"))]
        {
            // Neon is the lowest aarch64 CPU requirement (available in all Apple Silicon / Arm V7+).
            use aarch64::neon::l2_f32;
            return l2_f32(self, other);
        }

        // Fallback on x86_64 without AVX2 / FMA, or other platforms.
        #[cfg(not(target_arch = "aarch64"))]
        l2_scalar(self, other)
    }
}

impl L2 for Float32Array {
    type Output = f32;

    #[inline]
    fn l2(&self, other: &Float32Array) -> f32 {
        self.values().l2(other.values())
    }
}

/// Compute L2 distance between two vectors.
#[inline]
pub fn l2_distance(from: &[f32], to: &[f32]) -> f32 {
    from.l2(to)
}

/// Compute L2 distance between a vector and a batch of vectors.
///
/// Parameters
///
/// - `from`: the vector to compute distance from.
/// - `to`: a list of vectors to compute distance to.
/// - `dimension`: the dimension of the vectors.
pub fn l2_distance_batch(from: &[f32], to: &[f32], dimension: usize) -> Arc<Float32Array> {
    assert_eq!(from.len(), dimension);
    assert_eq!(to.len() % dimension, 0);

    let dists = unsafe {
        Float32Array::from_trusted_len_iter(to.chunks_exact(dimension).map(|v| Some(from.l2(v))))
    };
    Arc::new(dists)
}

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    pub(crate) mod avx {
        use super::super::l2_scalar;

        #[inline]
        pub(crate) fn l2_f32(from: &[f32], to: &[f32]) -> f32 {
            unsafe {
                use std::arch::x86_64::*;
                debug_assert_eq!(from.len(), to.len());

                // Get the potion of the vector that is aligned to 32 bytes.
                let len = from.len() / 8 * 8;
                let mut sums = _mm256_setzero_ps();
                for i in (0..len).step_by(8) {
                    let left = _mm256_loadu_ps(from.as_ptr().add(i));
                    let right = _mm256_loadu_ps(to.as_ptr().add(i));
                    let sub = _mm256_sub_ps(left, right);
                    // sum = sub * sub + sum
                    sums = _mm256_fmadd_ps(sub, sub, sums);
                }
                // Shift and add vector, until only 1 value left.
                // sums = [x0-x7], shift = [x4-x7]
                let mut shift = _mm256_permute2f128_ps(sums, sums, 1);
                // [x0+x4, x1+x5, ..]
                sums = _mm256_add_ps(sums, shift);
                shift = _mm256_permute_ps(sums, 14);
                sums = _mm256_add_ps(sums, shift);
                sums = _mm256_hadd_ps(sums, sums);
                let mut results: [f32; 8] = [0f32; 8];
                _mm256_storeu_ps(results.as_mut_ptr(), sums);

                // Remaining
                results[0] += l2_scalar(&from[len..], &to[len..]);
                results[0]
            }
        }
    }
}

#[cfg(target_arch = "aarch64")]
mod aarch64 {

    pub(super) mod neon {
        use super::super::l2_scalar;
        use std::arch::aarch64::*;

        #[inline]
        pub(crate) fn l2_f32(from: &[f32], to: &[f32]) -> f32 {
            unsafe {
                let len = from.len() / 4 * 4;
                let buf = [0.0_f32; 4];
                let mut sum = vld1q_f32(buf.as_ptr());
                for i in (0..len).step_by(4) {
                    let left = vld1q_f32(from.as_ptr().add(i));
                    let right = vld1q_f32(to.as_ptr().add(i));
                    let sub = vsubq_f32(left, right);
                    sum = vfmaq_f32(sum, sub, sub);
                }
                let mut sum = vaddvq_f32(sum);
                sum += l2_scalar(&from[len..], &to[len..]);
                sum
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use approx::assert_relative_eq;
    use arrow::array::{as_primitive_array, FixedSizeListArray};
    use arrow_array::types::Float32Type;

    #[test]
    fn test_euclidean_distance() {
        let mat = FixedSizeListArray::from_iter_primitive::<Float32Type, _, _>(
            vec![
                Some((0..8).map(|v| Some(v as f32)).collect::<Vec<_>>()),
                Some((1..9).map(|v| Some(v as f32)).collect::<Vec<_>>()),
                Some((2..10).map(|v| Some(v as f32)).collect::<Vec<_>>()),
                Some((3..11).map(|v| Some(v as f32)).collect::<Vec<_>>()),
            ],
            8,
        );
        let point = Float32Array::from((2..10).map(|v| Some(v as f32)).collect::<Vec<_>>());
        let scores = l2_distance_batch(
            point.values(),
            as_primitive_array::<Float32Type>(mat.values().as_ref()).values(),
            8,
        );

        assert_eq!(
            scores.as_ref(),
            &Float32Array::from(vec![32.0, 8.0, 0.0, 8.0])
        );
    }

    #[test]
    fn test_not_aligned() {
        let mat = (0..6)
            .chain(0..8)
            .chain(1..9)
            .chain(2..10)
            .chain(3..11)
            .map(|v| v as f32)
            .collect::<Vec<_>>();
        let point = Float32Array::from((0..10).map(|v| Some(v as f32)).collect::<Vec<_>>());
        let scores = l2_distance_batch(&point.values()[2..], &mat[6..], 8);

        assert_eq!(
            scores.as_ref(),
            &Float32Array::from(vec![32.0, 8.0, 0.0, 8.0])
        );
    }
    #[test]
    fn test_odd_length_vector() {
        let mat = Float32Array::from_iter((0..5).map(|v| Some(v as f32)));
        let point = Float32Array::from((2..7).map(|v| Some(v as f32)).collect::<Vec<_>>());
        let scores = l2_distance_batch(point.values(), mat.values(), 5);

        assert_eq!(scores.as_ref(), &Float32Array::from(vec![20.0]));
    }

    #[test]
    fn test_l2_distance_cases() {
        let values: Float32Array = vec![
            0.25335717, 0.24663818, 0.26330215, 0.14988247, 0.06042378, 0.21077952, 0.26687378,
            0.22145681, 0.18319066, 0.18688454, 0.05216244, 0.11470364, 0.10554603, 0.19964123,
            0.06387895, 0.18992095, 0.00123718, 0.13500804, 0.09516747, 0.19508345, 0.2582458,
            0.1211653, 0.21121833, 0.24809816, 0.04078768, 0.19586588, 0.16496408, 0.14766085,
            0.04898421, 0.14728612, 0.21263947, 0.16763233,
        ]
        .into();

        let q: Float32Array = vec![
            0.18549609,
            0.29954708,
            0.28318876,
            0.05424477,
            0.093134984,
            0.21580857,
            0.2951282,
            0.19866848,
            0.13868214,
            0.19819534,
            0.23271298,
            0.047727287,
            0.14394054,
            0.023316395,
            0.18589257,
            0.037315924,
            0.07037327,
            0.32609823,
            0.07344752,
            0.020155912,
            0.18485495,
            0.32763934,
            0.14296658,
            0.04498596,
            0.06254237,
            0.24348071,
            0.16009757,
            0.053892266,
            0.05918874,
            0.040363103,
            0.19913352,
            0.14545348,
        ]
        .into();

        let d = l2_distance_batch(q.values(), values.values(), 32);
        assert_relative_eq!(0.31935785197341404, d.value(0));
    }
}
