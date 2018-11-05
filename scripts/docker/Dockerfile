# Running command example 
# docker run tox-node 1D5A5F2F5D6233058BF0259B09622FB40B482E4FA0931EB8FD3AB8E7BF7DAF6F 198.98.51.198:33445
#
# arguments are bootstrap's publickey, Ip:port

FROM base/archlinux AS builder

USER root
WORKDIR /tmp/tox

RUN yes | pacman -Sy \
    llvm \
    make \
    automake \
    autoconf \
    gcc

ADD ./libsodium-1.0.13 ./

CMD cd libsodium-1.0.13
RUN ./configure
RUN make && make check
RUN make install

RUN curl https://sh.rustup.rs -sSf | sh -s -- -y && source $HOME/.cargo/env

ENV SODIUM_LIB_DIR=/usr/local/lib
ENV SODIUM_STATIC=1

ADD ./tox-node .

CMD cd tox-node

RUN $HOME/.cargo/bin/cargo build --release


FROM base/archlinux

WORKDIR /tmp/tox
COPY --from=builder \
    /tmp/tox/target/release/tox-node \
    .

EXPOSE 33445
EXPOSE 33445/udp
COPY ./start.sh .
ENTRYPOINT ["./start.sh"]
CMD []