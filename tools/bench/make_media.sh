#!/bin/sh
# The benchmark's media (ffmpeg): a 1080p60 "gameplay" video with sound, a 720p30 "webcam",
# a "mic" and music loop, a background, a logo and a webcam frame. Usage: make_media.sh DIR
set -e
cd "$1"
ffmpeg -v error -y -f lavfi -i "testsrc2=s=1920x1080:r=60,hue=H=t*0.5" -f lavfi -i "sine=f=220:r=48000,volume=0.2" -f lavfi -i "sine=f=330:r=48000,volume=0.15" -filter_complex "[1][2]amerge=inputs=2[a]" -map 0 -map "[a]" -t 60 -c:v libx264 -preset fast -b:v 12M -pix_fmt yuv420p -g 120 -c:a aac -b:a 192k gameplay.mp4
ffmpeg -v error -y -f lavfi -i "mandelbrot=s=1280x720:r=30" -t 60 -c:v libx264 -preset fast -b:v 4M -pix_fmt yuv420p -g 60 -an webcam.mp4
ffmpeg -v error -y -f lavfi -i "anoisesrc=c=pink:r=48000:a=0.08,tremolo=f=3:d=0.8" -t 60 -ac 1 -c:a aac -b:a 128k mic.m4a
ffmpeg -v error -y -f lavfi -i "sine=f=261.6:r=48000,volume=0.1" -f lavfi -i "sine=f=329.6:r=48000,volume=0.1" -f lavfi -i "sine=f=392:r=48000,volume=0.1" -filter_complex "[0][1][2]amix=inputs=3,apulsator=hz=0.5" -t 60 -ac 2 -c:a aac -b:a 192k music.m4a
ffmpeg -v error -y -f lavfi -i "gradients=s=1920x1080:c0=0x1a1040:c1=0x0a3050:x0=0:y0=0:x1=1920:y1=1080" -frames:v 1 background.png
ffmpeg -v error -y -f lavfi -i "color=c=0x00000000:s=400x400,format=rgba,geq=r='255':g='120':b='40':a='if(lt(hypot(X-200,Y-200),180),255,0)'" -frames:v 1 logo.png
ffmpeg -v error -y -f lavfi -i "color=c=black:s=520x310,format=rgba,geq=r='255':g='255':b='255':a='if(lt(X,20)+gt(X,499)+lt(Y,20)+gt(Y,289),255,0)'" -frames:v 1 camframe.png
