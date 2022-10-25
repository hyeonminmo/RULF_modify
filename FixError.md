# How to fix error message

## docker/docker-build 실행시 생기는 에러 메세지

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


```shell
sudo usermod -aG docker $USER
newgrp docker
./docker/docker-build
```

