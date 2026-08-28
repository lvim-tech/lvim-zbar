#!/bin/sh
# Instantaneous CPU % from two /proc/stat samples.
read -r _ u1 n1 s1 i1 w1 x1 y1 z1 _rest < /proc/stat
sleep 0.7
read -r _ u2 n2 s2 i2 w2 x2 y2 z2 _rest < /proc/stat
idle1=$((i1 + w1)); idle2=$((i2 + w2))
tot1=$((u1 + n1 + s1 + i1 + w1 + x1 + y1 + z1))
tot2=$((u2 + n2 + s2 + i2 + w2 + x2 + y2 + z2))
dt=$((tot2 - tot1)); di=$((idle2 - idle1))
[ "$dt" -le 0 ] && { echo "0%"; exit 0; }
echo "$(( (100 * (dt - di) + dt / 2) / dt ))%"
