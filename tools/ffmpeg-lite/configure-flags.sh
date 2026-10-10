#!/bin/sh
# FFmpeg "lite" for Simple Player: an ffmpeg.exe that can only *decode audio* and write raw
# PCM to a pipe. LGPL-2.1+ only (no --enable-gpl / --enable-nonfree), so it can ship with
# the Apache-2.0 app. Used by src/ffdec.rs for formats Symphonia can't read.
#
# Prints the configure flags; build.sh passes them to FFmpeg's ./configure.

DECODERS="
aac aac_fixed aac_latm alac ac3 ac3_fixed eac3 dca truehd mlp
ape wavpack tta mpc7 mpc8 tak
dsd_lsbf dsd_lsbf_planar dsd_msbf dsd_msbf_planar
wmav1 wmav2 wmapro wmalossless
opus vorbis flac mp1 mp1float mp2 mp2float mp3 mp3float mp3adu mp3adufloat
atrac3 atrac3p atrac9 cook ra_144 ra_288 als shorten
pcm_alaw pcm_mulaw pcm_s8 pcm_u8 pcm_s16le pcm_s16be pcm_s24le pcm_s24be pcm_s32le
pcm_s32be pcm_f32le pcm_f32be pcm_f64le pcm_f64be pcm_s16le_planar pcm_s24daud
adpcm_ima_wav adpcm_ms
"

DEMUXERS="
mov matroska ogg ape wv tta mpc mpc8 tak dsf iff asf
aac ac3 eac3 dts truehd mlp flac mp3 wav w64 aiff caf au rm
"

PARSERS="aac aac_latm ac3 dca flac mlp mpegaudio opus vorbis tak"

FILTERS="abuffer abuffersink aformat anull aresample atrim"

PROTOCOLS="file pipe http https tcp tls"

join() { echo "$1" | tr -s ' \n' '\n' | grep -v '^$' | paste -sd, -; }

echo "--disable-everything --disable-autodetect --disable-doc --disable-debug
--disable-ffprobe --disable-ffplay --enable-ffmpeg
--disable-avdevice --disable-swscale --disable-postproc
--enable-avformat --enable-avcodec --enable-avfilter --enable-swresample
--enable-network --enable-small
--enable-decoder=$(join "$DECODERS")
--enable-demuxer=$(join "$DEMUXERS")
--enable-parser=$(join "$PARSERS")
--enable-filter=$(join "$FILTERS")
--enable-protocol=$(join "$PROTOCOLS")
--enable-encoder=pcm_f32le --enable-muxer=pcm_f32le"
