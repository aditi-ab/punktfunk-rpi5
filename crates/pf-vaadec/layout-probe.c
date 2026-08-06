/*
 * Layout probe for the hand-declared libva structures in `src/va.rs`.
 *
 * `src/va.rs` declares VAAPI's decode buffers as `#[repr(C)]` Rust structs, because
 * this crate must build on macOS and in the Linux container where libva headers need
 * not exist. This file is how those declarations were CHECKED rather than eyeballed,
 * and it is committed so the check is reproducible instead of a claim in a commit
 * message.
 *
 * Run it against real headers (no Linux box required):
 *
 *   docker run --rm --platform linux/amd64 -v "$PWD/crates/pf-vaadec:/w" -w /w \
 *     pf-lxcheck2 bash -lc 'apt-get update -qq && apt-get install -y -qq libva-dev \
 *       && gcc -w -O0 layout-probe.c -o /tmp/probe && /tmp/probe'
 *
 * Every number it prints is pinned as a `const` assertion at the bottom of
 * `src/va.rs`, so a transcription mistake is a compile error. The bit-field section
 * exists because C bit-field allocation order is ABI-defined, not standardised:
 * it PROVES least-significant-bit-first on this ABI rather than assuming it.
 *
 * Last run against libva 2.23.0-1ubuntu1, x86_64-linux-gnu.
 */
#include <stdio.h>
#include <stddef.h>
#include <va/va.h>

#define S(t)        printf("size %-34s %zu align %zu\n", #t, sizeof(t), _Alignof(t))
#define O(t, f)     printf("off  %-20s %-28s %zu\n", #t, #f, offsetof(t, f))

int main(void) {
    S(VAPictureH264);
    O(VAPictureH264, picture_id);
    O(VAPictureH264, frame_idx);
    O(VAPictureH264, flags);
    O(VAPictureH264, TopFieldOrderCnt);
    O(VAPictureH264, BottomFieldOrderCnt);
    O(VAPictureH264, va_reserved);

    S(VAPictureParameterBufferH264);
    O(VAPictureParameterBufferH264, CurrPic);
    O(VAPictureParameterBufferH264, ReferenceFrames);
    O(VAPictureParameterBufferH264, picture_width_in_mbs_minus1);
    O(VAPictureParameterBufferH264, picture_height_in_mbs_minus1);
    O(VAPictureParameterBufferH264, bit_depth_luma_minus8);
    O(VAPictureParameterBufferH264, bit_depth_chroma_minus8);
    O(VAPictureParameterBufferH264, num_ref_frames);
    O(VAPictureParameterBufferH264, seq_fields);
    O(VAPictureParameterBufferH264, num_slice_groups_minus1);
    O(VAPictureParameterBufferH264, slice_group_map_type);
    O(VAPictureParameterBufferH264, slice_group_change_rate_minus1);
    O(VAPictureParameterBufferH264, pic_init_qp_minus26);
    O(VAPictureParameterBufferH264, pic_init_qs_minus26);
    O(VAPictureParameterBufferH264, chroma_qp_index_offset);
    O(VAPictureParameterBufferH264, second_chroma_qp_index_offset);
    O(VAPictureParameterBufferH264, pic_fields);
    O(VAPictureParameterBufferH264, frame_num);
    O(VAPictureParameterBufferH264, va_reserved);

    S(VAIQMatrixBufferH264);
    O(VAIQMatrixBufferH264, ScalingList4x4);
    O(VAIQMatrixBufferH264, ScalingList8x8);
    O(VAIQMatrixBufferH264, va_reserved);

    S(VASliceParameterBufferH264);
    O(VASliceParameterBufferH264, slice_data_size);
    O(VASliceParameterBufferH264, slice_data_offset);
    O(VASliceParameterBufferH264, slice_data_flag);
    O(VASliceParameterBufferH264, slice_data_bit_offset);
    O(VASliceParameterBufferH264, first_mb_in_slice);
    O(VASliceParameterBufferH264, slice_type);
    O(VASliceParameterBufferH264, direct_spatial_mv_pred_flag);
    O(VASliceParameterBufferH264, num_ref_idx_l0_active_minus1);
    O(VASliceParameterBufferH264, num_ref_idx_l1_active_minus1);
    O(VASliceParameterBufferH264, cabac_init_idc);
    O(VASliceParameterBufferH264, slice_qp_delta);
    O(VASliceParameterBufferH264, disable_deblocking_filter_idc);
    O(VASliceParameterBufferH264, slice_alpha_c0_offset_div2);
    O(VASliceParameterBufferH264, slice_beta_offset_div2);
    O(VASliceParameterBufferH264, RefPicList0);
    O(VASliceParameterBufferH264, RefPicList1);
    O(VASliceParameterBufferH264, luma_log2_weight_denom);
    O(VASliceParameterBufferH264, chroma_log2_weight_denom);
    O(VASliceParameterBufferH264, luma_weight_l0_flag);
    O(VASliceParameterBufferH264, luma_weight_l0);
    O(VASliceParameterBufferH264, luma_offset_l0);
    O(VASliceParameterBufferH264, chroma_weight_l0_flag);
    O(VASliceParameterBufferH264, chroma_weight_l0);
    O(VASliceParameterBufferH264, chroma_offset_l0);
    O(VASliceParameterBufferH264, luma_weight_l1_flag);
    O(VASliceParameterBufferH264, luma_weight_l1);
    O(VASliceParameterBufferH264, luma_offset_l1);
    O(VASliceParameterBufferH264, chroma_weight_l1_flag);
    O(VASliceParameterBufferH264, chroma_weight_l1);
    O(VASliceParameterBufferH264, chroma_offset_l1);
    O(VASliceParameterBufferH264, va_reserved);

    /* Bit-field allocation order: prove LSB-first rather than assume it. */
    {
        VAPictureParameterBufferH264 p;
        p.seq_fields.value = 0;
        p.seq_fields.bits.chroma_format_idc = 3;
        printf("bits seq_fields.chroma_format_idc=3 -> value 0x%08x\n", p.seq_fields.value);
        p.seq_fields.value = 0;
        p.seq_fields.bits.log2_max_frame_num_minus4 = 0xf;
        printf("bits seq_fields.log2_max_frame_num_minus4=0xf -> value 0x%08x\n", p.seq_fields.value);
        p.pic_fields.value = 0;
        p.pic_fields.bits.reference_pic_flag = 1;
        printf("bits pic_fields.reference_pic_flag=1 -> value 0x%08x\n", p.pic_fields.value);
        p.pic_fields.value = 0;
        p.pic_fields.bits.weighted_bipred_idc = 3;
        printf("bits pic_fields.weighted_bipred_idc=3 -> value 0x%08x\n", p.pic_fields.value);
    }
    printf("VA_PADDING_LOW=%d VA_PADDING_MEDIUM=%d\n", VA_PADDING_LOW, VA_PADDING_MEDIUM);
    return 0;
}
