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
#include <va/va_dec_hevc.h>
#include <va/va_drmcommon.h>

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

    /* ---- HEVC (va_dec_hevc.h) ---- */
    S(VAPictureHEVC);
    O(VAPictureHEVC, picture_id);
    O(VAPictureHEVC, pic_order_cnt);
    O(VAPictureHEVC, flags);
    O(VAPictureHEVC, va_reserved);

    S(VAPictureParameterBufferHEVC);
    O(VAPictureParameterBufferHEVC, CurrPic);
    O(VAPictureParameterBufferHEVC, ReferenceFrames);
    O(VAPictureParameterBufferHEVC, pic_width_in_luma_samples);
    O(VAPictureParameterBufferHEVC, pic_height_in_luma_samples);
    O(VAPictureParameterBufferHEVC, pic_fields);
    O(VAPictureParameterBufferHEVC, sps_max_dec_pic_buffering_minus1);
    O(VAPictureParameterBufferHEVC, bit_depth_luma_minus8);
    O(VAPictureParameterBufferHEVC, bit_depth_chroma_minus8);
    O(VAPictureParameterBufferHEVC, pcm_sample_bit_depth_luma_minus1);
    O(VAPictureParameterBufferHEVC, pcm_sample_bit_depth_chroma_minus1);
    O(VAPictureParameterBufferHEVC, log2_min_luma_coding_block_size_minus3);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_luma_coding_block_size);
    O(VAPictureParameterBufferHEVC, log2_min_transform_block_size_minus2);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_transform_block_size);
    O(VAPictureParameterBufferHEVC, log2_min_pcm_luma_coding_block_size_minus3);
    O(VAPictureParameterBufferHEVC, log2_diff_max_min_pcm_luma_coding_block_size);
    O(VAPictureParameterBufferHEVC, max_transform_hierarchy_depth_intra);
    O(VAPictureParameterBufferHEVC, max_transform_hierarchy_depth_inter);
    O(VAPictureParameterBufferHEVC, init_qp_minus26);
    O(VAPictureParameterBufferHEVC, diff_cu_qp_delta_depth);
    O(VAPictureParameterBufferHEVC, pps_cb_qp_offset);
    O(VAPictureParameterBufferHEVC, pps_cr_qp_offset);
    O(VAPictureParameterBufferHEVC, log2_parallel_merge_level_minus2);
    O(VAPictureParameterBufferHEVC, num_tile_columns_minus1);
    O(VAPictureParameterBufferHEVC, num_tile_rows_minus1);
    O(VAPictureParameterBufferHEVC, column_width_minus1);
    O(VAPictureParameterBufferHEVC, row_height_minus1);
    O(VAPictureParameterBufferHEVC, slice_parsing_fields);
    O(VAPictureParameterBufferHEVC, log2_max_pic_order_cnt_lsb_minus4);
    O(VAPictureParameterBufferHEVC, num_short_term_ref_pic_sets);
    O(VAPictureParameterBufferHEVC, num_long_term_ref_pic_sps);
    O(VAPictureParameterBufferHEVC, num_ref_idx_l0_default_active_minus1);
    O(VAPictureParameterBufferHEVC, num_ref_idx_l1_default_active_minus1);
    O(VAPictureParameterBufferHEVC, pps_beta_offset_div2);
    O(VAPictureParameterBufferHEVC, pps_tc_offset_div2);
    O(VAPictureParameterBufferHEVC, num_extra_slice_header_bits);
    O(VAPictureParameterBufferHEVC, st_rps_bits);
    O(VAPictureParameterBufferHEVC, va_reserved);

    S(VASliceParameterBufferHEVC);
    O(VASliceParameterBufferHEVC, slice_data_size);
    O(VASliceParameterBufferHEVC, slice_data_offset);
    O(VASliceParameterBufferHEVC, slice_data_flag);
    O(VASliceParameterBufferHEVC, slice_data_byte_offset);
    O(VASliceParameterBufferHEVC, slice_segment_address);
    O(VASliceParameterBufferHEVC, RefPicList);
    O(VASliceParameterBufferHEVC, LongSliceFlags);
    O(VASliceParameterBufferHEVC, collocated_ref_idx);
    O(VASliceParameterBufferHEVC, num_ref_idx_l0_active_minus1);
    O(VASliceParameterBufferHEVC, num_ref_idx_l1_active_minus1);
    O(VASliceParameterBufferHEVC, slice_qp_delta);
    O(VASliceParameterBufferHEVC, slice_cb_qp_offset);
    O(VASliceParameterBufferHEVC, slice_cr_qp_offset);
    O(VASliceParameterBufferHEVC, slice_beta_offset_div2);
    O(VASliceParameterBufferHEVC, slice_tc_offset_div2);
    O(VASliceParameterBufferHEVC, luma_log2_weight_denom);
    O(VASliceParameterBufferHEVC, delta_chroma_log2_weight_denom);
    O(VASliceParameterBufferHEVC, delta_luma_weight_l0);
    O(VASliceParameterBufferHEVC, luma_offset_l0);
    O(VASliceParameterBufferHEVC, delta_chroma_weight_l0);
    O(VASliceParameterBufferHEVC, ChromaOffsetL0);
    O(VASliceParameterBufferHEVC, delta_luma_weight_l1);
    O(VASliceParameterBufferHEVC, luma_offset_l1);
    O(VASliceParameterBufferHEVC, delta_chroma_weight_l1);
    O(VASliceParameterBufferHEVC, ChromaOffsetL1);
    O(VASliceParameterBufferHEVC, five_minus_max_num_merge_cand);
    O(VASliceParameterBufferHEVC, num_entry_point_offsets);
    O(VASliceParameterBufferHEVC, entry_offset_to_subset_array);
    O(VASliceParameterBufferHEVC, slice_data_num_emu_prevn_bytes);
    O(VASliceParameterBufferHEVC, va_reserved);

    S(VAIQMatrixBufferHEVC);
    O(VAIQMatrixBufferHEVC, ScalingList4x4);
    O(VAIQMatrixBufferHEVC, ScalingList8x8);
    O(VAIQMatrixBufferHEVC, ScalingList16x16);
    O(VAIQMatrixBufferHEVC, ScalingList32x32);
    O(VAIQMatrixBufferHEVC, ScalingListDC16x16);
    O(VAIQMatrixBufferHEVC, ScalingListDC32x32);
    O(VAIQMatrixBufferHEVC, va_reserved);

    {
        VAPictureParameterBufferHEVC h;
        h.pic_fields.value = 0;
        h.pic_fields.bits.chroma_format_idc = 3;
        printf("bits hevc pic_fields.chroma_format_idc=3 -> 0x%08x\n", h.pic_fields.value);
        h.pic_fields.value = 0;
        h.pic_fields.bits.NoBiPredFlag = 1;
        printf("bits hevc pic_fields.NoBiPredFlag=1 -> 0x%08x\n", h.pic_fields.value);
        h.slice_parsing_fields.value = 0;
        h.slice_parsing_fields.bits.IntraPicFlag = 1;
        printf("bits hevc slice_parsing_fields.IntraPicFlag=1 -> 0x%08x\n", h.slice_parsing_fields.value);
        VASliceParameterBufferHEVC s;
        s.LongSliceFlags.value = 0;
        s.LongSliceFlags.fields.slice_type = 3;
        printf("bits hevc LongSliceFlags.slice_type=3 -> 0x%08x\n", s.LongSliceFlags.value);
        s.LongSliceFlags.value = 0;
        s.LongSliceFlags.fields.slice_loop_filter_across_slices_enabled_flag = 1;
        printf("bits hevc LongSliceFlags.slice_loop_filter_across=1 -> 0x%08x\n", s.LongSliceFlags.value);
    }

    printf("VA_PADDING_LOW=%d VA_PADDING_MEDIUM=%d\n", VA_PADDING_LOW, VA_PADDING_MEDIUM);

    /*
     * The export descriptor. This one is not a buffer we FILL — it is a struct the
     * driver WRITES, so a wrong layout is read as plausible garbage (an fd from the
     * middle of a pitch, a plane count from a modifier's high word) rather than
     * refused. It carries fixed-size arrays whose bounds the flattening walk trusts,
     * which is exactly the shape that turned into the green-screen bug once already.
     */
    S(VADRMPRIMESurfaceDescriptor);
    O(VADRMPRIMESurfaceDescriptor, fourcc);
    O(VADRMPRIMESurfaceDescriptor, width);
    O(VADRMPRIMESurfaceDescriptor, height);
    O(VADRMPRIMESurfaceDescriptor, num_objects);
    O(VADRMPRIMESurfaceDescriptor, objects);
    O(VADRMPRIMESurfaceDescriptor, num_layers);
    O(VADRMPRIMESurfaceDescriptor, layers);
    printf("count VADRMPRIMESurfaceDescriptor objects   %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->objects) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->objects[0]));
    printf("count VADRMPRIMESurfaceDescriptor layers    %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0]));
    printf("count layer.object_index                    %zu\n",
           sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0].object_index) /
               sizeof(((VADRMPRIMESurfaceDescriptor *)0)->layers[0].object_index[0]));

    /*
     * The enumerators the runtime calls pass by value. Printed rather than
     * transcribed because two of them are the exact pair this program has already
     * been warned about: VASliceParameterBufferType and VASliceDataBufferType are
     * 4 and 5, not the 3 and 4 that counting the enum from the top suggests.
     */
    printf("enum VAEntrypointVLD                        %d\n", VAEntrypointVLD);
    printf("enum VAConfigAttribRTFormat                 %d\n", VAConfigAttribRTFormat);
    printf("enum VAPictureParameterBufferType           %d\n", VAPictureParameterBufferType);
    printf("enum VAIQMatrixBufferType                   %d\n", VAIQMatrixBufferType);
    printf("enum VASliceParameterBufferType             %d\n", VASliceParameterBufferType);
    printf("enum VASliceDataBufferType                  %d\n", VASliceDataBufferType);
    printf("enum VA_EXPORT_SURFACE_READ_ONLY            0x%04x\n", VA_EXPORT_SURFACE_READ_ONLY);
    printf("enum VA_EXPORT_SURFACE_SEPARATE_LAYERS      0x%04x\n",
           VA_EXPORT_SURFACE_SEPARATE_LAYERS);
    printf("enum VA_SURFACE_ATTRIB_SETTABLE             0x%04x\n", VA_SURFACE_ATTRIB_SETTABLE);
    printf("enum VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2 0x%08x\n",
           VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2);
    printf("enum VASurfaceAttribPixelFormat             %d\n", VASurfaceAttribPixelFormat);
    printf("enum VAGenericValueTypeInteger              %d\n", VAGenericValueTypeInteger);
    printf("enum VA_STATUS_SUCCESS                      %d\n", VA_STATUS_SUCCESS);
    printf("enum VA_INVALID_ID                          0x%08x\n", VA_INVALID_ID);
    printf("enum VA_FOURCC_NV12                         0x%08x\n", VA_FOURCC_NV12);
    printf("enum VA_FOURCC_P010                         0x%08x\n", VA_FOURCC_P010);
    printf("enum VA_PROGRESSIVE                         0x%04x\n", VA_PROGRESSIVE);

    S(VASurfaceAttrib);
    O(VASurfaceAttrib, type);
    O(VASurfaceAttrib, flags);
    O(VASurfaceAttrib, value);
    S(VAGenericValue);
    O(VAGenericValue, type);
    O(VAGenericValue, value);
    S(VAConfigAttrib);
    O(VAConfigAttrib, type);
    O(VAConfigAttrib, value);
    return 0;
}
