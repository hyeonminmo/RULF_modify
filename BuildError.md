# How to fix error message

## docker/docker-build 실행 시 생기는 에러 메세지

```shell
sudo ./docker/docker-build
```

Error message
```shell
...
 ---> Running in a3f351faf2b9
groupadd: GID '0' already exists
useradd: UID 0 is not unique
...
unable to find user jjf: no matching entries in passwd file
...
```

Solution

local 에서 sudo 권한을 root 모드에서 user 모드으로 변경

```shell
sudo usermod -aG docker $USER
newgrp docker
./docker/docker-build
```

