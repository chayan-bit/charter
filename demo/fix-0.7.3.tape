# Batch 0.7.3, live on the Arkiv Rabdi testnet: clone with a git older than 2.42 (Ubuntu 22.04 ships 2.34).
# OLD is a directory holding the released v0.7.2 `chart` and `git-remote-arkiv`, NEW the same two built from this branch:
#   OLD=<dir> NEW=<dir> vhs demo/fix-0.7.3.tape      (or nix-shell -p vhs --run 'vhs demo/fix-0.7.3.tape')
# The rendered files belong on the demo-assets branch.
Output "/tmp/fix-0.7.3.mp4"
Output "/tmp/fix-0.7.3.gif"

Require git

Set Shell bash
Set FontSize 16
Set Width 1280
Set Height 720
Set Framerate 12
Set TypingSpeed 25ms
Set Padding 24

Hide
Type "export PS1='> ' U=arkiv::zBZ3MRTENmgPBebpq97ZPTbxeFZNaS8VX6Es25hpdJsSJ CHARTER_HOME=$(mktemp -d) && cd $(mktemp -d) && clear"
Enter
Show

Type "# Charter's own repo, cloned live from Arkiv with the system git"
Enter
Type "git --version"
Enter
Wait
Sleep 1.5s

Type "# v0.7.2: git rev-list --stdin refuses --not before git 2.42"
Enter
Type "PATH=$OLD:$PATH; chart --version; git clone -q $U charter"
Enter
Wait@60s
Sleep 3s

Type "# this branch: exclusions go in as ^<oid>, which every git reads"
Enter
Type "PATH=$NEW:${PATH#$OLD:}; rm -rf $CHARTER_HOME/*; git clone -q $U charter"
Enter
Wait@60s
Type "git -C charter log --oneline -3; git -C charter fsck --no-progress && echo fsck ok"
Enter
Wait@30s
Sleep 4s
