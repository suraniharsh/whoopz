# The newest version of every shell Whoopz knows about (Arch is a rolling release).
FROM archlinux:latest
RUN pacman -Syu --noconfirm --needed \
        bash zsh fish dash tcsh nushell xonsh elvish \
        coreutils findutils grep sed procps-ng \
    && pacman -Scc --noconfirm
