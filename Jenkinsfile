def CARGO_PATH = "/home/jenkins-docker/cache/.cargo"

pipeline {

	// 使用 Docker 代理而不是任意代理
	// docker run -u 指定用户id ，那么这里运行的容器内已uid为指定uid的用户运行，docker容器内要预先创建好这个uid的用户否则的话只有uid没有名字， 容器内程序若在挂载的目录创建文件或文件夹，在宿主机上的所属用户的uid等同于-u指定的用户id
    agent {
		docker {
			image 'rust-1.90-mine' // 使用 Rust 镜像
            reuseNode true // 重用工作空间，避免文件复制
             // 会默认使用与宿主机 jenkins服务 相同用户的 UID/GID(可以使用 id jenkins 获得)来运行docker
             // 工作空间是docker内部的工作空间， 这里要保证的是宿主机 jenkins服务用户对/var/cache/cargo的所有权 (使用chown -R jenkins /var/cache/cargo 将目录所有权改为jenkins)， 还要保证在docker容器内对应的用户 最好是${CARGO_PATH}整个目录的所有者
             // 这里容器内${CARGO_PATH}这个父目录是不存在的，所以会被docker服务先创建，由于docker服务是root的，那么${CARGO_PATH}的所有者是root的，但是registry的所有者会被docker设置为 -u 所指定的用户，也就是默认的jenkins
//             args "-v /var/cache/cargo:${CARGO_PATH}/registry"
            // 懒得改Dockerfile，直接挂载整个目录，那么其所有者就会是jenkins用户
             args "-v /var/cache/cargo:${CARGO_PATH}"
             //todo 将编译的target目录也挂载, 我也不希望每次都cargo update
        }
    }

    environment {
        SOURCE_DIR = "${WORKSPACE}/src"
        PROJECT_NAME = "my-rust-app"
        CARGO_HOME = "${CARGO_PATH}"
        PATH = "${CARGO_HOME}/bin:${PATH}"
    }

    stages {
        stage('Setup') {
            steps {
                sh '''
                    echo "工作空间是: ${WORKSPACE}"
                    echo "Current user: $(whoami)"
                    echo "User ID: $(id -u)"
                    echo "Setting up Cargo directories..."
                    ls -la ${WORKSPACE}
                    ls -la ${CARGO_HOME}
                    # 创建源代码目录
                    mkdir -p ${SOURCE_DIR}
                    ls -la ${SOURCE_DIR}
                    mkdir -p ${CARGO_HOME}/registry
                    mkdir -p ${CARGO_HOME}/git
                    ls -la ${CARGO_HOME}
                    echo "Directories created successfully"
                    echo "Directory ownership:"
                    ls -la ${WORKSPACE}
                    echo "Source dir ownership:"
                    ls -la ${SOURCE_DIR} 2>/dev/null || echo "Source dir not exists"
                '''
            }
        }

		stage('Checkout') {
			steps {
				 dir("${SOURCE_DIR}") {
                     // 使用更保守的Git设置
                     checkout([
                         $class: 'GitSCM',
                         branches: [[name: 'main']],
                         extensions: [
                             [$class: 'RelativeTargetDirectory', relativeTargetDir: '.'],
                             [$class: 'CleanCheckout'],  // 只清理目标目录，不是整个工作空间
                             [$class: 'CloneOption', noTags: false, shallow: true, depth: 1]
                         ],
                         userRemoteConfigs: [[
                             credentialsId: 'github-login',
                             url: 'git@github.com:cyqry/real_time_ctrl.git'
                         ]]
                     ])
                 }
            }
        }
        stage('Update'){
        	steps {
        	      dir("${SOURCE_DIR}") {
                     sh 'cargo update'
                  }
            }
        }
        stage('Build') {
			steps {
			     dir("${SOURCE_DIR}") {
			    	// 直接在容器中构建，无需手动设置环境
                    sh 'cargo build -p ctrl_server --verbose  --release --target x86_64-unknown-linux-musl'

                    // 列出构建产物，确认位置
                    sh 'ls -la'
                 }
            }
        }

//         stage('Test') {
// 			steps {
// 			     dir("${SOURCE_DIR}") {
// 			     	sh 'cargo test --verbose'
//                  }
//             }
//         }


//     可能是需要也可能不需要 先在远程服务器上创建一个 与jenkins服务uid一致的用户并将要用到的文件夹的所有者设置为该用户
        stage('Test SSH Connection') {
			steps {
				script {
					withCredentials([sshUserPrivateKey(
                        credentialsId: 'ytycc-server-1',
                        keyFileVariable: 'SSH_KEY',
                        usernameVariable: 'SSH_USER'
                    )]) {
						sh """
						    ssh -i $SSH_KEY -o StrictHostKeyChecking=no root@ytycc.com whoami
                            ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@ytycc.com "echo 'SSH connection successful'"
                        """
                    }
                }
            }
        }

        stage('Deploy Modules') {
	    		steps {
	    			script {
	    				def deployments = [
                    [name: 'ctrl_server',
                     host: 'ytycc.com',
                     credId: 'ytycc-server-1',
                     path: '/home/jenkins-deploy/rust/ctrl_server/',
                     scriptPath: '/home/jenkins-deploy/rust/reboot.sh'
                     ]
                    ]

                    deployments.each { dep ->
                        echo "Starting deployment of ${dep.name} to ${dep.host}"

                        try {
	    					if (dep.host == "local") {
	    						sh """
                                    mkdir -p ${dep.path} || true
                                    cp -v ${SOURCE_DIR}/target/x86_64-unknown-linux-musl/release/${dep.name} ${dep.path}
                                    echo "Local deployment completed successfully"
                                """
                            } else {
	    						withCredentials([sshUserPrivateKey(
                                    credentialsId: dep.credId,
                                    keyFileVariable: 'SSH_KEY',
                                    usernameVariable: 'SSH_USER'
                                )]) {
	    							sh """
	                                    # 清理旧文件
                                        echo "Cleaning ${dep.name} of ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} 'rm -f ${dep.path}${dep.name}'

                                        # 复制文件
                                        echo "Copying ${dep.name} to ${dep.host}"
                                        scp -i $SSH_KEY -o StrictHostKeyChecking=no -v ${SOURCE_DIR}/target/x86_64-unknown-linux-musl/release/${dep.name} $SSH_USER@${dep.host}:${dep.path}

                                        # 设置权限
                                        echo "Setting permissions on ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} 'chmod +x ${dep.path}${dep.name}'

                                        # 验证部署
                                        echo "Verifying deployment on ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} 'ls -la ${dep.path}${dep.name}'

                                        echo "Deployment to ${dep.host} completed successfully"

                                        echo "Publish on ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} '${dep.scriptPath}'

                                        echo "Publish on ${dep.host} completed successfully"
                                    """
                                }
                            }
                        } catch (Exception e) {
	    					echo "Deployment failed: ${e.getMessage()}"
                            currentBuild.result = 'FAILURE'
                            // 可选：发送通知
                        }
                    }
                }
            }
        }
    }

    post {
        always {
            // 使用双引号解析变量
            archiveArtifacts artifacts: "src/target/x86_64-unknown-linux-musl/release/ctrl_server", fingerprint: true
            cleanWs() // 清理工作空间
        }
    }
}