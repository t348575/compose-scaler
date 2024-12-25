# compose-scaler ![Visits](https://https://lambda.348575.xyz/repo-view-counter?repo=compose-scaler)
Start and stop (delete) docker compose projects on demand through traefik. Heavily inspired by [sablier](https://github.com/acouvreur/sablier).

## Getting started
* Clone the repository
* Build the container
* Add to your static traefik config:
```yaml
experimental:
  localPlugins:
    compose_scaler:
      moduleName: "github.com/t348575/compose-scaler"
```
* Mount the repo path to the traefik container:
```yaml
- /path/to/repo/compose-scaler:/plugins-local/src/github.com/t348575/compose-scaler
```
* Sample docker compose config:
```yaml
version: '3'
services:
  compose-scaler:
    container_name: compose-scaler
    image: compose-scaler
    # port to listen on, config file path
    command: ["-a", "0.0.0.0:10000", "-c", "/config.yaml"]
    volumes:
    # uses docker socket to get container health / status
      - /var/run/docker.sock:/var/run/docker.sock
    # path to public & private key for ssh-ing back to host
      - /home/my_user/.ssh/compose_scaler_key.pub:/compose_scaler_key.pub:ro
      - /home/my_user/.ssh/compose_scaler_key:/compose_scaler_key:ro
    # compose-scaler config file
      - ./compose-scaler.yaml:/config.yaml:ro
```

* Sample compose-scaler config:
Confiugration for projects here is compulsory, but not in the traefik middlewares file. Only name is compulsory, all other fields have defaults.
```yaml
ssh_host: my_host:22
ssh_username: my_user
projects_dir: /path/on/host/to/compose/projects
public_key: /compose_scaler_key.pub
private_key: /compose_scaler_key
projects:
  - name: gitea
    name: dockge # this must match the folder name inside your compose projects directory
    displayName: Dockge # displayed on the waiting page
    refreshFrequency: 5s # how often to refresh status, etc.
    sessionDuration: 5m # time before running docker compose down
    # if required, pass a path to a custom docker compose, eg. overleaf uses one
    # the default is "docker compose"
    customCommand: bin/docker-compose
```

* Expected compose projects directory structure:
```
├── my_project_dir
│   ├── gitea
|   |   ├── docker-compose.yaml
│   ├── dockge
|   |   ├── docker-compose.yaml
│   ├── overleaf
|   |   ├── anything_inside
```

* Sample traefik middleware config:
This configuration is not compulsory, only the name is compulsory here. It overrides any configuration set in the compose scaler config yaml.
```yaml
http:
  middlewares:
    dockge_cs:
      plugin:
        compose_scaler:
          name: dockge # this must match the folder name inside your compose projects directory
          displayName: Dockge # displayed on the waiting page
          refreshFrequency: 5s # how often to refresh status, etc.
          theme: ghost # theme for waiting page
          composeScalerUrl: http://compose-scaler:10000 # compose-scaler uri
          sessionDuration: 5m # time before running docker compose down
          # if required, pass a path to a custom docker compose, eg. overleaf uses one
          # the default is "docker compose"
          customCommand: bin/docker-compose
```