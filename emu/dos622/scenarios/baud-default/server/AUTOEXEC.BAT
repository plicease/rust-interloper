@ECHO OFF
PROMPT $p$g
PATH C:\DOS
SET TEMP=C:\DOS
ECHO INTERSVR server: sharing C: (C:\FIXTURE) and B: (fixture.img, write-protected).
INTERSVR C: B: /COM:1
