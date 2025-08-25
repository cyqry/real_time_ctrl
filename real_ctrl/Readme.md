```

// c-s-c 结构
// 如果没有命令连接，那么每隔10秒尝试连接服务端
// 与服务器的连接分为: 控制端，被控端，数据传输连接
// 除了命令和其返回结果，任何数据的传输使用新建的连接
// 控制端发送控制命令后，需要等待服务器返回(OK帧或者Err帧)，包含content或错误信息，并由控制端自己处理这个content和错误信息
// 效果:
//   控制端:
//   $开头的为自动识别命令
//     $sys_list -> (服务端返回信息)返回被控列表
//     $sys_use ...-> (服务端判断)为这条控制连接设置当前被控者，任意不以$sys_开头的命令，必须被设置了控制者后发送才生效，否则返回错误信息(服务端返回)
//     $local_exit ... -> (本地判断)直接断开连接，输出结束控制信息
//     $getfile "D:\\aa" to "E:\\bb"  -> 将被控端的aa传输到本地的bb，该命令只支持500M以内的文件
//     $getbigfile "" to ""  -> 将被控端的大文件逐渐写入本地，实现较难
//     $setfile "E:\\bb" to "D:\\aa" -> 将本地文件写入被控端 , 该命令只支持500M以内的文件
//     $ls "D:\\"  -> 返回给定目录的子目录列表（每一项详细信息:目录名或文件名,为目录还是文件，文件大小，为目录的话要展示目录下级目录或文件的数量文件的话展示文件大小带单位,全路径，）

// let commands = vec![
//     "$sys_list",
//     "$sys_use \"config\"",
//     "$local_exit",
//     "$getfile \"D:\\aa\" to \"E:\\bb\"",
//     "$getbigfile \"D:\\cc\" to \"E:\\dd\"",
//     "$setfile \"E:\\bb\" to \"D:\\aa\"",
//     "$ls \"D:\\\"",
//     "echo Hello, World!"
// ];
//
// for command in commands {
//     println!("{}", command);
//     match command.parse::<Command>() {
//         Ok(cmd) => println!("{:?}", cmd),
//         Err(e) => println!("Error: {}", e),
//     }
// }
```