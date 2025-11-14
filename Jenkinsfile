pipeline {
	// 使用 Docker 代理而不是任意代理
    agent {
		docker {
			image 'rust:1-slim-buster' // 使用官方 Rust 镜像
            reuseNode true // 重用工作空间，避免文件复制
             // 会默认使用与宿主机 jenkins服务 相同用户的 UID/GID(可以使用 id jenkins 获得)来运行docker
             // 工作空间是docker内部的工作空间， 这里要保证的是宿主机 jenkins服务用户对/var/cache/cargo的所有权
//             args '-v /var/cache/cargo:${WORKSPACE}/.cargo/registry'
        }
    }

    environment {
        SOURCE_DIR = "${WORKSPACE}/src"
        PROJECT_NAME = "my-rust-app"
        CARGO_HOME = "${WORKSPACE}/.cargo"
        CARGO_TARGET_DIR = "${WORKSPACE}/target"
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
                    # 创建源代码目录
                    mkdir -p ${SOURCE_DIR}
                    mkdir -p ${CARGO_HOME}
                    mkdir -p ${CARGO_HOME}/registry
                    mkdir -p ${CARGO_HOME}/git
                    mkdir -p ${CARGO_TARGET_DIR}
                    echo "Directories created successfully"
                    echo "Directory ownership:"
                    ls -la /var/lib/jenkins/workspace/real_time_ctrl/
                    echo "Source dir ownership:"
                    ls -la /var/lib/jenkins/workspace/real_time_ctrl/src/ 2>/dev/null || echo "Source dir not exists"

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

        stage('Build') {
			steps {
			     dir("${SOURCE_DIR}") {
			    	// 直接在容器中构建，无需手动设置环境
			        sh 'cargo update'
                    sh 'cargo build -p ctrl_server --verbose  --release'

                    // 列出构建产物，确认位置
                    sh 'ls -la target/release/'
                 }
            }
        }

        stage('Test') {
			steps {
			     dir("${SOURCE_DIR}") {
			     	sh 'cargo test --verbose'
                 }
            }
        }
        stage('Test SSH Connection') {
			steps {
				script {
					withCredentials([sshUserPrivateKey(
                        credentialsId: 'ssh-deploy-key',
                        keyFileVariable: 'SSH_KEY',
                        usernameVariable: 'SSH_USER'
                    )]) {
						sh """
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
                     path: '/home/rust/ctrl_server/']
                    ]

                    deployments.each { dep ->
                        echo "Starting deployment of ${dep.name} to ${dep.host}"

                        try {
	    					if (dep.host == "local") {
	    						sh """
                                    mkdir -p ${dep.path} || true
                                    cp -v target/release/${dep.name} ${dep.path}
                                    echo "Local deployment completed successfully"
                                """
                            } else {
	    						withCredentials([sshUserPrivateKey(
                                    credentialsId: dep.credId,
                                    keyFileVariable: 'SSH_KEY',
                                    usernameVariable: 'SSH_USER'
                                )]) {
	    							sh """
                                        # 复制文件
                                        echo "Copying ${dep.name} to ${dep.host}"
                                        scp -i $SSH_KEY -o StrictHostKeyChecking=no -v ${SOURCE_DIR}/target/release/${dep.name} $SSH_USER@${dep.host}:${dep.path}

                                        # 设置权限
                                        echo "Setting permissions on ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} 'chmod +x ${dep.path}${dep.name}'

                                        # 验证部署
                                        echo "Verifying deployment on ${dep.host}"
                                        ssh -i $SSH_KEY -o StrictHostKeyChecking=no $SSH_USER@${dep.host} 'ls -la ${dep.path}${dep.name} && ${dep.path}${dep.name} --version'

                                        echo "Deployment to ${dep.host} completed successfully"
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
			// 可选：清理或存档构建产物
            archiveArtifacts artifacts: '${SOURCE_DIR}/target/release/*', fingerprint: true
            cleanWs() // 清理工作空间
        }
    }
}